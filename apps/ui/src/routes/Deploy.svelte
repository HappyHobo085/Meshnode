<script lang="ts">
  import { onMount, tick } from "svelte";
  import {
    api,
    toUiError,
    type DeployOptsDto,
    type PlacementDto,
    type Platform,
    type ServiceDto,
    type UiError,
  } from "../lib/api";
  import Button from "../lib/design/Button.svelte";
  import Field from "../lib/design/Field.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import Sheet from "../lib/design/Sheet.svelte";
  import { bytes } from "../lib/format";
  import { hrefOf } from "../lib/router";
  import {
    DEFAULT_LIMITS,
    displayPath,
    hostMembers,
    NAME_RULE,
    nameFromPath,
    placementReady,
    validServiceName,
  } from "../lib/service";
  import { members, refreshNow, services, toast } from "../lib/stores";
  import PlacementEditor from "./PlacementEditor.svelte";

  interface Props {
    open: boolean;
    onclose: () => void;
  }
  let { open, onclose }: Props = $props();

  type Kind = ServiceDto["kind"];
  type Step = "source" | "name" | "placement" | "sending" | "done";
  const STEPS: { step: Step; label: string }[] = [
    { step: "source", label: "Choose" },
    { step: "name", label: "Name" },
    { step: "placement", label: "Placement" },
  ];

  const uid = $props.id();
  const nameId = `deploy-name-${uid}`;

  let platform = $state<Platform | null>(null);
  let platformReady: Promise<Platform | null> = Promise.resolve(null);
  onMount(() => {
    platformReady = api.platform().catch(() => null);
    void platformReady.then((p) => (platform = p));
  });

  let step = $state<Step>("source");
  let source = $state<{ kind: Kind; path: string } | null>(null);
  let name = $state("");
  let nameError = $state<string | null>(null);
  let placement = $state<PlacementDto>({ kind: "allHosts" });
  let placementTouched = $state(false);
  let picking = $state<Kind | null>(null);
  let sent = $state(0);
  let error = $state<UiError | null>(null);
  /** Each deploy's id, so a late progress report from an earlier one is ignored. */
  let run = 0;

  const existing = $derived($services.find((s) => s.name === name.trim()) ?? null);
  const hosts = $derived(hostMembers($members));
  const stepIndex = $derived(STEPS.findIndex((s) => s.step === step));

  /** Back to the first step; a deploy still running finishes on its own (see `deploy`). */
  function reset() {
    run++;
    step = "source";
    source = null;
    name = "";
    nameError = null;
    placement = { kind: "allHosts" };
    placementTouched = false;
    sent = 0;
    error = null;
  }

  // Every opening starts at the first step.
  $effect(() => {
    if (open) reset();
  });

  async function focus(id: string) {
    await tick();
    document.getElementById(id)?.focus();
  }

  async function pick(kind: Kind) {
    picking = kind;
    error = null;
    try {
      const p = platform ?? (await platformReady);
      let path: string | null;
      if (p === "android") {
        path = kind === "static" ? await api.android.pickTree() : await api.android.pickWasm();
      } else {
        path =
          kind === "static" ? await api.desktop.pickFolder() : await api.desktop.pickFile("wasm");
      }
      // Cancelling the picker is a choice, not an error: stay here, quietly.
      if (!path) return;
      source = { kind, path };
      name = nameFromPath(path);
      nameError = null;
      step = "name";
      void focus(nameId);
    } catch (e) {
      const err = toUiError(e);
      if (err.code !== "pickerCancelled" && err.code !== "cancelled") error = err;
    } finally {
      picking = null;
    }
  }

  function toPlacement(e?: Event) {
    e?.preventDefault();
    const n = name.trim();
    if (!validServiceName(n)) {
      nameError = NAME_RULE;
      return;
    }
    name = n;
    nameError = null;
    // A new version of an existing service keeps its placement unless changed here.
    if (!placementTouched) placement = existing?.placement ?? { kind: "allHosts" };
    step = "placement";
  }

  async function deploy() {
    if (!source) return;
    const id = ++run;
    const { kind, path } = source;
    const target = name;
    const opts: DeployOptsDto = {
      // Redeploying keeps the service's limits and outbound list (edited in its detail).
      limits: existing?.limits ?? DEFAULT_LIMITS,
      placement,
      outbound: existing?.outbound ?? [],
    };
    const onProgress = (n: number) => {
      if (id === run) sent = n;
    };
    sent = 0;
    error = null;
    step = "sending";
    try {
      const p = platform ?? (await platformReady);
      if (kind === "wasm") {
        await api.deployWasmFile(path, target, opts);
      } else if (p === "android") {
        await api.android.deployStaticTree(path, target, opts, onProgress);
      } else {
        await api.desktop.deployStaticDir(path, target, opts, onProgress);
      }
      await refreshNow();
      // Closed (or started over) while it ran: the sheet can't say so, a toast does.
      if (id !== run || !open) toast(`Deployed ${target}`);
      if (id === run) step = "done";
    } catch (e) {
      if (id !== run) return;
      error = toUiError(e);
      step = "placement";
    }
  }

  const another = reset;
</script>

<Sheet {open} title="Deploy a service" wide {onclose}>
  <div class="deploy">
    {#if stepIndex >= 0}
      <ol class="steps" aria-label="Steps">
        {#each STEPS as s, i (s.step)}
          <li
            class={{ done: i < stepIndex, current: i === stepIndex }}
            aria-current={i === stepIndex ? "step" : undefined}
          >
            <span class="dot num"
              >{#if i < stepIndex}<Icon name="check" size={12} />{:else}{i + 1}{/if}</span
            >
            {s.label}
          </li>
        {/each}
      </ol>
    {/if}

    {#if source && (step === "name" || step === "placement")}
      <div class="picked">
        <Icon name={source.kind === "static" ? "folder" : "file"} size={18} />
        <span class="picked-path mono truncate" title={source.path}>{displayPath(source.path)}</span
        >
        <button type="button" class="change" onclick={another}>Change</button>
      </div>
    {/if}

    {#if step === "source"}
      <div class="sources">
        <button
          type="button"
          class="source"
          disabled={picking !== null}
          aria-busy={picking === "static" || undefined}
          onclick={() => pick("static")}
        >
          <span class="source-icon"><Icon name="folder" size={26} /></span>
          <span class="source-title">A site folder</span>
          <span class="source-help">HTML, CSS and images, served as they are.</span>
        </button>
        <button
          type="button"
          class="source"
          disabled={picking !== null}
          aria-busy={picking === "wasm" || undefined}
          onclick={() => pick("wasm")}
        >
          <span class="source-icon"><Icon name="file" size={26} /></span>
          <span class="source-title">A .wasm module</span>
          <span class="source-help">A program that answers HTTP requests.</span>
        </button>
      </div>
      {#if error}
        <p class="error" role="alert">
          {error.message}{#if error.hint}<span class="hint">{error.hint}</span>{/if}
        </p>
      {/if}
    {:else if step === "name"}
      <form class="form" onsubmit={toPlacement} novalidate>
        <Field
          id={nameId}
          label="Service name"
          bind:value={name}
          mono
          autocomplete="off"
          autocapitalize="off"
          spellcheck={false}
          help={existing
            ? `Replaces ${existing.name} v${existing.version} with v${existing.version + 1}. Its limits and outbound list stay.`
            : "Lowercase letters, digits and dashes. You'll see it in addresses and logs."}
          error={nameError}
        />
        <div class="actions">
          <Button variant="ghost" onclick={another}>Back</Button>
          <Button variant="primary" type="submit">Next</Button>
        </div>
      </form>
    {:else if step === "placement"}
      <div class="form">
        <div class="where">
          <h3>Where should <span class="mono">{name}</span> run?</h3>
          <PlacementEditor
            value={placement}
            onchange={(p) => {
              placement = p;
              placementTouched = true;
            }}
            {hosts}
            service={name}
          />
        </div>
        {#if error}
          <div class="failed" role="alert">
            <strong>Couldn't deploy {name}</strong>
            <span class="failed-msg">{error.message}</span>
            {#if error.hint}<span class="hint">{error.hint}</span>{/if}
          </div>
        {/if}
        <div class="actions">
          <Button variant="ghost" onclick={() => (step = "name")}>Back</Button>
          <Button variant="primary" disabled={!placementReady(placement)} onclick={deploy}>
            <Icon name="upload" size={18} />Deploy {name}
          </Button>
        </div>
      </div>
    {:else if step === "sending" && source}
      <div class="sending" aria-live="polite">
        <h3>Deploying <span class="mono">{name}</span></h3>
        <div
          class="bar"
          role="progressbar"
          aria-label="Deploying {name}"
          aria-valuetext={source.kind === "static" ? `${bytes(sent)} read` : "Sending the module"}
        >
          <span class="bar-fill"></span>
        </div>
        <p class="progress-text num">
          {source.kind === "static" ? `${bytes(sent)} read` : "Sending the module"}
        </p>
      </div>
    {:else if step === "done"}
      <div class="done">
        <span class="tick"><Icon name="check" size={24} /></span>
        <h3>{name} is deployed</h3>
        <p>Its hosts fetch it next and start it; each one shows its state in the service.</p>
        <div class="actions">
          <Button variant="ghost" onclick={another}>Deploy another</Button>
          <a class="open" href={hrefOf({ name: "service", params: { name } })}>Open {name}</a>
          <Button variant="primary" onclick={onclose}>Done</Button>
        </div>
      </div>
    {/if}
  </div>
</Sheet>

<style>
  .deploy {
    display: grid;
    gap: var(--s-5);
    min-width: 0;
  }

  /* The three steps: a real sequence, so numbered. */
  .steps {
    display: flex;
    gap: var(--s-5);
    margin: 0;
    padding: 0;
    list-style: none;
    color: var(--ink-3);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
  }
  .steps li {
    display: flex;
    align-items: center;
    gap: var(--s-2);
  }
  .dot {
    display: grid;
    place-items: center;
    width: 22px;
    height: 22px;
    border-radius: 50%;
    border: 1px solid var(--line-strong);
    font-size: var(--fs-xs);
  }
  .steps .current {
    color: var(--ink);
  }
  .steps .current .dot {
    border-color: var(--signal);
    background: var(--signal);
    color: var(--signal-ink);
  }
  .steps .done .dot {
    border-color: transparent;
    background: var(--signal-soft);
    color: var(--signal);
  }

  .picked {
    display: flex;
    align-items: center;
    gap: var(--s-2);
    min-width: 0;
    padding: var(--s-1) var(--s-1) var(--s-1) var(--s-3);
    border-radius: var(--r-sm);
    background: var(--surface-2);
    color: var(--ink-2);
  }
  .picked-path {
    flex: 1;
    font-size: var(--fs-sm);
  }
  .change {
    flex: none;
    min-height: 32px;
    padding: 0 var(--s-3);
    border: 0;
    border-radius: var(--r-xs);
    background: transparent;
    color: var(--signal);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    cursor: pointer;
  }
  .change:hover {
    background: var(--surface-3);
  }
  @media (pointer: coarse) {
    .change {
      min-height: var(--touch);
    }
  }

  /* The two kinds of thing you can deploy, as large choices. */
  .sources {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(100%, 220px), 1fr));
    gap: var(--s-3);
  }
  .source {
    display: grid;
    justify-items: start;
    align-content: start;
    gap: var(--s-1);
    min-height: 140px;
    padding: var(--s-5);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-md);
    background: var(--surface);
    color: var(--ink);
    text-align: left;
    cursor: pointer;
    transition:
      border-color var(--dur-1) var(--ease-out),
      background-color var(--dur-1) var(--ease-out),
      transform var(--dur-1) var(--ease-out);
  }
  .source:hover:not(:disabled) {
    border-color: var(--signal);
    background: color-mix(in oklab, var(--signal-soft) 50%, var(--surface));
  }
  .source:active:not(:disabled) {
    transform: translateY(1px);
  }
  .source:disabled {
    cursor: progress;
    opacity: 0.6;
  }
  .source[aria-busy="true"] {
    opacity: 1;
    border-color: var(--signal);
  }
  .source:focus-visible {
    border-radius: var(--r-md);
  }
  .source-icon {
    display: grid;
    place-items: center;
    width: 48px;
    height: 48px;
    margin-bottom: var(--s-2);
    border-radius: var(--r-sm);
    background: var(--signal-soft);
    color: var(--signal);
  }
  .source-title {
    font-weight: var(--fw-semibold);
  }
  .source-help {
    color: var(--ink-2);
    font-size: var(--fs-sm);
  }

  .form {
    display: grid;
    gap: var(--s-5);
  }
  .where {
    display: grid;
    gap: var(--s-3);
  }
  .where h3,
  .sending h3 {
    font-size: var(--fs-md);
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    align-items: center;
    gap: var(--s-3);
  }

  .error {
    font-size: var(--fs-sm);
    color: var(--danger);
  }
  .hint {
    display: block;
    color: var(--ink-2);
  }
  .failed {
    display: grid;
    gap: 2px;
    padding: var(--s-3) var(--s-4);
    border-radius: var(--r-sm);
    background: var(--danger-soft);
    font-size: var(--fs-sm);
  }
  .failed strong {
    color: var(--danger);
    font-weight: var(--fw-semibold);
  }
  .failed-msg {
    color: var(--ink);
    overflow-wrap: anywhere;
  }

  /* Progress: the total isn't known until every file is read, so the bar moves
     without claiming a percentage; the bytes read so far are the honest number. */
  .sending {
    display: grid;
    gap: var(--s-3);
    padding: var(--s-4) 0 var(--s-2);
  }
  .bar {
    position: relative;
    height: 6px;
    border-radius: var(--r-pill);
    background: var(--surface-3);
    overflow: hidden;
  }
  .bar-fill {
    position: absolute;
    inset: 0 auto 0 0;
    width: 36%;
    border-radius: inherit;
    background: var(--signal);
    animation: travel 1.4s var(--ease-in-out) infinite;
  }
  @keyframes travel {
    from {
      transform: translateX(-100%);
    }
    to {
      transform: translateX(280%);
    }
  }
  /* Without motion, a still, full-width bar: work is under way, no pretend position. */
  @media (prefers-reduced-motion: reduce) {
    .bar-fill {
      width: 100%;
      opacity: 0.45;
      animation: none;
    }
  }
  .progress-text {
    color: var(--ink-2);
    font-size: var(--fs-sm);
  }

  .done {
    display: grid;
    justify-items: start;
    gap: var(--s-2);
  }
  .done h3 {
    font-size: var(--fs-lg);
    overflow-wrap: anywhere;
  }
  .done p {
    color: var(--ink-2);
    font-size: var(--fs-sm);
  }
  .done .actions {
    justify-self: stretch;
    margin-top: var(--s-4);
  }
  .tick {
    display: grid;
    place-items: center;
    width: 44px;
    height: 44px;
    margin-bottom: var(--s-2);
    border-radius: 50%;
    background: var(--ok);
    color: var(--surface);
    animation: mesh-arrive var(--dur-3) var(--ease-out) both;
  }
  /* "Open" is a link (it changes the route) drawn as a secondary button. */
  .open {
    display: inline-flex;
    align-items: center;
    min-height: var(--touch);
    padding: 0 var(--s-5);
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    background: var(--surface-2);
    color: var(--ink);
    font-weight: var(--fw-medium);
    text-decoration: none;
  }
  .open:hover {
    background: var(--surface-3);
  }
  .open:focus-visible {
    border-radius: var(--r-sm);
  }

  @media (max-width: 639px) {
    .steps {
      gap: var(--s-3);
    }
    .actions > :global(*) {
      flex: 1;
      justify-content: center;
    }
  }
</style>
