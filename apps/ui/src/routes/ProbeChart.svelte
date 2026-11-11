<script lang="ts">
  import { onMount } from "svelte";
  import { api, type MemberDto, type ProbeSampleDto } from "../lib/api";
  import Button from "../lib/design/Button.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import Monogram from "../lib/design/Monogram.svelte";
  import { rtt } from "../lib/format";
  import { openLive, type Live, type LiveState } from "../lib/live";
  import { niceCeiling, PROBE, summarize } from "../lib/probe";

  interface Props {
    /** The other devices in the mesh (this one can't be probed). */
    devices: MemberDto[];
  }
  let { devices }: Props = $props();

  const uid = $props.id();
  let chosenId = $state<string | null>(null);
  /** The device the samples on screen belong to. */
  let target = $state<MemberDto | null>(null);
  let samples: ProbeSampleDto[] = $state([]);
  let stream = $state<LiveState>({ kind: "idle" });
  let live: Live | null = null;

  const chosen = $derived(devices.find((d) => d.id === chosenId) ?? null);
  const running = $derived(stream.kind === "connecting" || stream.kind === "live");
  const summary = $derived(summarize(samples));

  // Chart geometry, in viewBox units. Everything is an SVG attribute: the CSP allows no
  // inline style strings.
  // The viewBox follows the figure's real width, so labels stay 12 px on a phone instead of
  // shrinking with a scaled-down drawing (640 before layout, and in tests).
  let chartWidth = $state(0);
  const W = $derived(Math.max(280, Math.round(chartWidth) || 640));
  const H = 200;
  const PAD = { top: 14, right: 16, bottom: 26, left: 52 };
  const plotW = $derived(W - PAD.left - PAD.right);
  const plotH = H - PAD.top - PAD.bottom;

  const slots = $derived(Math.max(PROBE.count, samples.length));
  const ceiling = $derived(niceCeiling(summary.max ?? 0));
  const x = (i: number) => PAD.left + (slots <= 1 ? plotW / 2 : (i / (slots - 1)) * plotW);
  const y = (ms: number) => PAD.top + plotH - (Math.min(ms, ceiling) / ceiling) * plotH;

  const points = $derived(
    samples.flatMap((s, i) =>
      s.error === null && s.ms !== null ? [{ i, ms: s.ms, cx: x(i), cy: y(s.ms) }] : [],
    ),
  );
  const fails = $derived(
    samples.flatMap((s, i) => (s.error !== null || s.ms === null ? [{ i, cx: x(i) }] : [])),
  );
  const line = $derived(points.map((p) => `${p.cx.toFixed(1)},${p.cy.toFixed(1)}`).join(" "));
  const grid = $derived([0, ceiling / 2, ceiling]);

  const chartLabel = $derived(
    target
      ? `Latency to ${target.name}: ${summary.ok} answered, ${summary.failed} failed` +
          (summary.p50 !== null ? `, median ${rtt(summary.p50)}` : "")
      : "Latency chart",
  );

  function start() {
    const device = chosen;
    if (!device) return;
    live?.close();
    target = device;
    samples = [];
    live = openLive(
      (onEnd) =>
        api.probe({ nodePrefix: device.id, ...PROBE }, (s) => (samples = [...samples, s]), onEnd),
      (s) => (stream = s),
    );
  }

  function stop() {
    live?.close();
    live = null;
    stream = { kind: "ended" };
  }

  onMount(() => () => {
    live?.close();
    live = null;
  });
</script>

<div class="probe">
  {#if devices.length === 0}
    <p class="muted">Add another device to measure the latency to it.</p>
  {:else}
    <fieldset class="targets">
      <legend class="visually-hidden">Device to measure</legend>
      {#each devices as d (d.id)}
        <label class={["target", { checked: chosenId === d.id }]} style:--h={d.accentHue}>
          <input
            class="visually-hidden"
            type="radio"
            name="probe-target-{uid}"
            value={d.id}
            bind:group={chosenId}
            aria-describedby={d.online ? undefined : `probe-off-${uid}-${d.id}`}
          />
          <Monogram name={d.name} hue={d.accentHue} size="sm" online={d.online} />
          <span class="truncate">{d.name}</span>
          {#if !d.online}
            <span class="off" id="probe-off-{uid}-{d.id}">offline</span>
          {/if}
        </label>
      {/each}
    </fieldset>

    <div class="run">
      {#if running}
        <Button onclick={stop}><Icon name="pause" size={16} />Stop</Button>
        <span class="progress num" role="status">
          {stream.kind === "connecting"
            ? "Starting…"
            : `Sample ${samples.length} of ${PROBE.count}`}
        </span>
      {:else}
        <Button variant="primary" onclick={start} disabled={!chosen}>
          {samples.length > 0 && target?.id === chosenId ? "Measure again" : "Measure latency"}
        </Button>
        <span class="progress">
          {PROBE.count} pings, one a second
        </span>
      {/if}
    </div>
  {/if}

  {#if stream.kind === "disconnected"}
    <div class="strip lost" role="status">
      <Icon name="alert" size={16} />
      <p>
        <strong>Disconnected</strong>
        <span class="detail">{stream.error?.hint ?? "The measurement stopped."}</span>
      </p>
      <Button size="sm" onclick={start}><Icon name="refresh" size={16} />Resume</Button>
    </div>
  {:else if stream.kind === "busy" || stream.kind === "failed"}
    <div class="strip failed" role="alert">
      <Icon name="alert" size={16} />
      <p>
        <strong>{stream.error.message}</strong>
        {#if stream.kind === "busy"}
          <span class="detail">Close the Logs screen or another latency view, then try again.</span>
        {:else if stream.error.hint}
          <span class="detail">{stream.error.hint}</span>
        {/if}
      </p>
    </div>
  {/if}

  {#if target && (samples.length > 0 || running)}
    <figure class="chart" style:--h={target.accentHue}>
      <svg viewBox="0 0 {W} {H}" bind:clientWidth={chartWidth} role="img" aria-label={chartLabel}>
        {#each grid as g, i (i)}
          <line class="grid" x1={PAD.left} x2={W - PAD.right} y1={y(g)} y2={y(g)} />
          <text class="axis" x={PAD.left - 10} y={y(g)} dy="0.32em" text-anchor="end"
            >{g === 0 ? "0" : rtt(g)}</text
          >
        {/each}
        {#if summary.p50 !== null}
          <line
            class="median"
            x1={PAD.left}
            x2={W - PAD.right}
            y1={y(summary.p50)}
            y2={y(summary.p50)}
          />
        {/if}
        {#if points.length > 1}
          <polyline class="trace" points={line} />
        {/if}
        {#each points as p (p.i)}
          <circle class="sample" cx={p.cx} cy={p.cy} r="3.5">
            <title>{rtt(p.ms)}</title>
          </circle>
        {/each}
        {#each fails as f (f.i)}
          <path
            class="fail"
            d="M{f.cx - 4} {PAD.top + plotH - 4}l8 8m0 -8l-8 8"
            transform="translate(0 -4)"
          >
            <title>No answer</title>
          </path>
        {/each}
      </svg>
      <figcaption class="legend">
        <span class="key"><span class="swatch dot"></span>Answer</span>
        <span class="key"><span class="swatch dash"></span>Median</span>
        <span class="key"><span class="swatch cross" aria-hidden="true">×</span>No answer</span>
      </figcaption>
    </figure>

    <ul class="summary" aria-label="Latency summary">
      <li><span class="label">p50</span><span class="value num">{rtt(summary.p50)}</span></li>
      <li><span class="label">p95</span><span class="value num">{rtt(summary.p95)}</span></li>
      <li><span class="label">max</span><span class="value num">{rtt(summary.max)}</span></li>
      <li class={{ bad: summary.failed > 0 }}>
        <span class="label">failed</span><span class="value num">{summary.failed}</span>
      </li>
    </ul>
  {/if}
</div>

<style>
  .probe {
    display: grid;
    gap: var(--s-4);
    min-width: 0;
  }
  .muted {
    color: var(--ink-3);
    font-size: var(--fs-sm);
  }
  .targets {
    display: flex;
    flex-wrap: wrap;
    gap: var(--s-2);
    margin: 0;
    padding: 0;
    border: 0;
    min-width: 0;
  }
  .target {
    display: inline-flex;
    align-items: center;
    gap: var(--s-2);
    max-width: 100%;
    min-height: 40px;
    padding: var(--s-1) var(--s-3) var(--s-1) var(--s-1);
    border: 1px solid var(--line);
    border-radius: var(--r-pill);
    background: var(--surface);
    color: var(--ink);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    cursor: pointer;
    transition:
      border-color var(--dur-1) var(--ease-out),
      background-color var(--dur-1) var(--ease-out);
  }
  .target:hover {
    border-color: var(--line-strong);
  }
  .target.checked {
    border-color: var(--accent);
    background: var(--accent-soft);
    color: var(--accent-ink);
  }
  .target:has(input:focus-visible) {
    box-shadow: var(--focus-ring);
  }
  .off {
    color: var(--ink-3);
    font-weight: var(--fw-regular);
  }
  .run {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--s-3);
  }
  .progress {
    color: var(--ink-3);
    font-size: var(--fs-sm);
  }

  .strip {
    display: flex;
    align-items: center;
    gap: var(--s-3);
    padding: var(--s-2) var(--s-2) var(--s-2) var(--s-4);
    border-radius: var(--r-sm);
    color: var(--ink);
    font-size: var(--fs-sm);
  }
  .strip p {
    flex: 1;
    min-width: 0;
  }
  .lost {
    background: var(--warn-soft);
  }
  .lost > :global(.icon) {
    color: var(--warn);
  }
  .failed {
    background: var(--danger-soft);
  }
  .failed > :global(.icon) {
    color: var(--danger);
  }
  .detail {
    color: var(--ink-2);
  }

  /* The chart takes the device's hue: its trace is that device's colour, as on Home. */
  .chart {
    display: grid;
    gap: var(--s-2);
    margin: 0;
    padding: var(--s-3) var(--s-3) var(--s-2);
    border: 1px solid var(--line);
    border-radius: var(--r-md);
    background: var(--surface);
  }
  svg {
    width: 100%;
    height: auto;
    overflow: visible;
  }
  .grid {
    stroke: var(--line);
    stroke-width: 1;
  }
  .axis {
    fill: var(--ink-3);
    font-size: 12px;
    font-family: var(--font-mono);
  }
  .median {
    stroke: var(--ink-3);
    stroke-width: 1;
    stroke-dasharray: 4 4;
  }
  /* --accent-ink, not --accent: it holds contrast on both paper and graphite at every hue. */
  .trace {
    fill: none;
    stroke: var(--accent-ink);
    stroke-width: 2;
    stroke-linejoin: round;
    stroke-linecap: round;
  }
  .sample {
    fill: var(--surface);
    stroke: var(--accent-ink);
    stroke-width: 2;
    animation: mesh-arrive var(--dur-2) var(--ease-out) both;
    transform-box: fill-box;
    transform-origin: center;
  }
  .fail {
    fill: none;
    stroke: var(--danger);
    stroke-width: 2;
    stroke-linecap: round;
  }
  .legend {
    display: flex;
    flex-wrap: wrap;
    gap: var(--s-1) var(--s-4);
    padding-left: var(--s-2);
    color: var(--ink-3);
    font-size: var(--fs-xs);
  }
  .key {
    display: inline-flex;
    align-items: center;
    gap: var(--s-1);
  }
  .swatch.dot {
    width: 8px;
    height: 8px;
    border: 2px solid var(--accent-ink);
    border-radius: 50%;
  }
  .swatch.dash {
    width: 14px;
    border-top: 1px dashed var(--ink-3);
  }
  .swatch.cross {
    color: var(--danger);
    font-weight: var(--fw-bold);
  }

  .summary {
    display: grid;
    grid-template-columns: repeat(4, minmax(0, 1fr));
    margin: 0;
    padding: 0;
    list-style: none;
    border: 1px solid var(--line);
    border-radius: var(--r-md);
    overflow: hidden;
  }
  .summary li {
    display: grid;
    gap: 2px;
    padding: var(--s-3) var(--s-4);
    background: var(--surface);
  }
  .summary li + li {
    border-left: 1px solid var(--line);
  }
  .label {
    color: var(--ink-3);
    font-size: var(--fs-xs);
  }
  .value {
    font-size: var(--fs-lg);
    font-weight: var(--fw-semibold);
    letter-spacing: -0.01em;
  }
  .bad .value {
    color: var(--danger);
  }

  @media (pointer: coarse) {
    .target {
      min-height: var(--touch);
    }
  }
  @media (max-width: 479px) {
    .summary {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
    .summary li:nth-child(3) {
      border-left: 0;
    }
    .summary li:nth-child(n + 3) {
      border-top: 1px solid var(--line);
    }
  }
</style>
