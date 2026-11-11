<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "../lib/api";
  import Button from "../lib/design/Button.svelte";
  import Chip from "../lib/design/Chip.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import { openLive, type Live, type LiveState } from "../lib/live";
  import {
    LEVELS,
    LOG_CAPACITY,
    LOG_HISTORY,
    levelOf,
    splitMatches,
    type Level,
  } from "../lib/logs";
  import { RingBuffer } from "../lib/ringbuffer";

  interface Entry {
    /** Arrival number: a stable key. */
    n: number;
    text: string;
    level: Level;
  }

  /** Every row is one line high, so the list can be windowed by arithmetic. */
  const ROW = 20;
  /** Rows drawn beyond each edge of the viewport, so fast scrolling doesn't flash. */
  const OVERSCAN = 12;
  /** The viewport height assumed before layout (and in tests, which have none). */
  const FALLBACK_HEIGHT = 600;

  const LABEL: Record<Level, string> = {
    trace: "Trace",
    debug: "Debug",
    info: "Info",
    warn: "Warn",
    error: "Error",
  };

  const buffer = new RingBuffer<Entry>(LOG_CAPACITY);
  let seq = 0;
  let lastLevel: Level = "info";
  /**
   * The buffer itself is not reactive. A new line bumps `version` only if no bump is waiting
   * to be read, so a burst of lines costs one state write, not one per line; `seen` (read
   * by the flush) re-arms it.
   */
  let version = $state(0);
  let bumpWaiting = false;
  function changed() {
    if (!bumpWaiting) {
      bumpWaiting = true;
      version++;
    }
  }
  // Deliberate side effect: reading `seen` re-arms `changed`. Every view value below reads
  // it, and the template reads them each flush; keep it that way when refactoring, or a
  // burst could leave the view one bump behind.
  const seen = $derived.by(() => {
    bumpWaiting = false;
    return version;
  });

  let live: Live | null = null;
  let stream = $state<LiveState>({ kind: "idle" });

  let paused = $state(false);
  let frozen: Entry[] = $state.raw([]);
  /** Lines that arrived while paused (plain: read through `seen`). */
  let pendingCount = 0;
  const pending = $derived.by(() => {
    void seen;
    return paused ? pendingCount : 0;
  });

  let shown: Record<Level, boolean> = $state({
    trace: true,
    debug: true,
    info: true,
    warn: true,
    error: true,
  });
  let query = $state("");

  let viewport: HTMLElement | undefined = $state();
  let viewportHeight = $state(0);
  let scrollTop = $state(0);
  let follow = $state(true);

  const needle = $derived(query.trim().toLowerCase());
  const filtering = $derived(needle !== "" || LEVELS.some((l) => !shown[l]));
  /** How many lines the view holds (live, or as frozen by Pause). */
  const total = $derived.by(() => {
    void seen;
    return paused ? frozen.length : buffer.length;
  });
  /** Only a filter needs every line; otherwise the view reads just its window. */
  const matches: Entry[] | null = $derived.by(() => {
    if (!filtering) return null;
    void seen;
    const all = paused ? frozen : buffer.toArray();
    return all.filter(
      (e) => shown[e.level] && (needle === "" || e.text.toLowerCase().includes(needle)),
    );
  });
  const count = $derived(matches ? matches.length : total);

  const rowsInView = $derived(Math.ceil((viewportHeight || FALLBACK_HEIGHT) / ROW));
  const first = $derived(
    follow
      ? Math.max(0, count - rowsInView)
      : Math.min(Math.floor(scrollTop / ROW), Math.max(0, count - rowsInView)),
  );
  const from = $derived(Math.max(0, first - OVERSCAN));
  const to = $derived(first + rowsInView + OVERSCAN);
  const slice: Entry[] = $derived.by(() => {
    void seen;
    if (matches) return matches.slice(from, to);
    return paused ? frozen.slice(from, to) : buffer.slice(from, to);
  });

  /** One element per row the viewport can show, plus the overscan on each side. */
  const pool = $derived(Array.from({ length: rowsInView + 2 * OVERSCAN }, (_, i) => i));
  const rowEls: (HTMLElement | undefined)[] = [];
  function row(slot: number) {
    return (el: HTMLElement) => {
      rowEls[slot] = el;
      return () => {
        if (rowEls[slot] === el) rowEls[slot] = undefined;
      };
    };
  }

  /** Draws one line into a pooled row: plain text, or text with the search hits marked
   *  (built from DOM nodes, never HTML strings). */
  function paint(el: HTMLElement, entry: Entry | undefined, q: string) {
    if (!entry) {
      if (!el.hidden) {
        el.hidden = true;
        el.textContent = "";
        delete el.dataset.level;
      }
      return;
    }
    el.hidden = false;
    if (el.dataset.level !== entry.level) el.dataset.level = entry.level;
    if (!q) {
      if (el.firstChild?.nodeType !== Node.TEXT_NODE || el.childNodes.length !== 1) {
        el.textContent = entry.text;
      } else if (el.firstChild.nodeValue !== entry.text) {
        el.firstChild.nodeValue = entry.text;
      }
      return;
    }
    el.replaceChildren(
      ...splitMatches(entry.text, q).map((p) => {
        if (!p.hit) return document.createTextNode(p.text);
        const mark = document.createElement("mark");
        mark.textContent = p.text;
        return mark;
      }),
    );
  }

  $effect(() => {
    const rows = slice;
    const q = needle;
    for (let i = 0; i < pool.length; i++) {
      const el = rowEls[i];
      if (el) paint(el, rows[i], q);
    }
  });

  const countText = $derived(
    filtering
      ? `${count.toLocaleString()} of ${total.toLocaleString()} lines`
      : `${total.toLocaleString()} ${total === 1 ? "line" : "lines"}`,
  );

  /**
   * Set while (re)connecting: the lines on screen stay until the new stream's first line
   * arrives, so a Resume that fails again keeps the lines that explain the disconnect.
   */
  let replaceOnFirstLine = false;

  function onLine(text: string) {
    if (replaceOnFirstLine) {
      // The fresh stream replays the recent history, so the old lines go now.
      replaceOnFirstLine = false;
      buffer.clear();
      lastLevel = "info";
      pendingCount = 0;
    }
    const level = levelOf(text) ?? lastLevel;
    lastLevel = level;
    buffer.push({ n: seq++, text, level });
    // Only the last LOG_CAPACITY lines are kept, so more can't be waiting.
    if (paused) pendingCount = Math.min(pendingCount + 1, LOG_CAPACITY);
    changed();
  }

  function connect() {
    live?.close();
    replaceOnFirstLine = true;
    live = openLive(
      (onEnd) => api.logsFollow(LOG_HISTORY, onLine, onEnd),
      (s) => (stream = s),
    );
  }

  /** "Disconnected — Resume": reconnect and follow again (a paused view would stay frozen). */
  function resume() {
    if (paused) {
      paused = false;
      pendingCount = 0;
      follow = true;
      changed();
    }
    connect();
  }

  onMount(() => {
    connect();
    return () => {
      live?.close();
      live = null;
    };
  });

  function togglePause() {
    pendingCount = 0;
    if (paused) {
      paused = false;
      follow = true;
    } else {
      frozen = buffer.toArray();
      paused = true;
    }
  }

  function onscroll() {
    if (!viewport) return;
    scrollTop = viewport.scrollTop;
    follow = viewport.scrollTop + viewport.clientHeight >= viewport.scrollHeight - ROW;
  }

  function jumpToLatest() {
    follow = true;
  }

  // While following, keep the newest line in view.
  $effect(() => {
    void slice;
    if (follow && viewport) viewport.scrollTop = viewport.scrollHeight;
  });

  const lostText = $derived(
    stream.kind === "disconnected"
      ? (stream.error?.hint ?? "The log stream stopped.")
      : "The log stream ended.",
  );
</script>

<section class="logs">
  <header class="head">
    <div class="title">
      <h1>Logs</h1>
      <p class="sub">
        <span class={["live", { on: stream.kind === "live" && !paused, paused }]} aria-hidden="true"
        ></span>
        <span class="state"
          >{paused
            ? "Paused"
            : stream.kind === "live"
              ? "Live"
              : stream.kind === "connecting"
                ? "Connecting…"
                : stream.kind === "disconnected" || stream.kind === "ended"
                  ? "Disconnected"
                  : "Not following"}</span
        >
        <span class="count num">{countText}</span>
        {#if paused && pending > 0}
          <span class="pending num"
            >{pending.toLocaleString()} new {pending === 1 ? "line" : "lines"}</span
          >
        {/if}
      </p>
    </div>
    <Button size="sm" onclick={togglePause} aria-pressed={paused}>
      <Icon name={paused ? "play" : "pause"} size={16} />
      {paused ? "Resume" : "Pause"}
    </Button>
  </header>

  <div class="toolbar">
    <label class="search">
      <Icon name="search" size={16} />
      <input
        type="search"
        aria-label="Search logs"
        placeholder="Search"
        autocomplete="off"
        spellcheck="false"
        bind:value={query}
      />
    </label>
    <div class="levels" role="group" aria-label="Levels">
      {#each LEVELS as level (level)}
        <Chip
          label={LABEL[level]}
          selected={shown[level]}
          onclick={() => (shown[level] = !shown[level])}
        />
      {/each}
    </div>
  </div>

  {#if stream.kind === "disconnected" || stream.kind === "ended"}
    <div class="strip lost" role="status">
      <Icon name="alert" size={16} />
      <p><strong>Disconnected</strong> <span class="detail">{lostText}</span></p>
      <Button size="sm" onclick={resume}><Icon name="refresh" size={16} />Resume</Button>
    </div>
  {:else if stream.kind === "busy" || stream.kind === "failed"}
    <div class="strip failed" role="alert">
      <Icon name="alert" size={16} />
      <p>
        <strong>{stream.error.message}</strong>
        {#if stream.kind === "busy"}
          <span class="detail">Close another log or latency view, then try again.</span>
        {:else if stream.error.hint}
          <span class="detail">{stream.error.hint}</span>
        {/if}
      </p>
      <Button size="sm" onclick={resume}>Try again</Button>
    </div>
  {/if}

  <div class="panel">
    <!-- A scrollable region must be reachable by keyboard to scroll it (WCAG 2.1.1). -->
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
    <div
      class="viewport"
      bind:this={viewport}
      bind:clientHeight={viewportHeight}
      {onscroll}
      role="log"
      aria-label="Meshnode log"
      aria-live="off"
      tabindex="0"
    >
      <div class="spacer" style:height="{count * ROW}px">
        <div class="rows" style:transform="translateY({from * ROW}px)">
          <!-- A fixed pool of row elements: new lines and scrolling only repaint their text
               (see `paint`), as a virtual list should; no per-row reactive state. -->
          {#each pool as slot (slot)}
            <div class="log-row" {@attach row(slot)}></div>
          {/each}
        </div>
      </div>
      {#if count === 0}
        <p class="empty">
          {stream.kind === "connecting"
            ? "Opening the log…"
            : total === 0
              ? "No log lines yet."
              : "No lines match. Clear the search or turn a level back on."}
        </p>
      {/if}
    </div>
    {#if !follow && count > 0}
      <button type="button" class="jump" onclick={jumpToLatest}>
        <Icon name="chevron" size={14} />
        Jump to latest
      </button>
    {/if}
  </div>
</section>

<style>
  /* A column whose last child, the tail, takes all the height left. */
  .logs {
    display: flex;
    flex-direction: column;
    gap: var(--s-4);
    height: calc(100dvh - var(--s-8) - var(--s-12));
    min-height: 420px;
  }
  .logs > * {
    min-width: 0;
  }
  .head {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: var(--s-4);
  }
  .title {
    display: grid;
    gap: var(--s-1);
    min-width: 0;
  }
  .sub {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--s-1) var(--s-3);
    font-size: var(--fs-sm);
    color: var(--ink-3);
  }
  .state {
    color: var(--ink-2);
    font-weight: var(--fw-medium);
  }
  .live {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--line-strong);
  }
  .live.on {
    background: var(--ok);
    animation: mesh-pulse 2s var(--ease-out) infinite;
  }
  .live.paused {
    background: var(--warn);
  }
  .pending {
    color: var(--warn);
    font-weight: var(--fw-medium);
  }

  .toolbar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--s-3);
  }
  .search {
    display: flex;
    align-items: center;
    gap: var(--s-2);
    flex: 1 1 240px;
    min-width: 0;
    min-height: 36px;
    padding: 0 var(--s-3);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    background: var(--surface);
    color: var(--ink-3);
  }
  .search:focus-within {
    border-color: var(--signal);
    box-shadow: 0 0 0 3px var(--signal-soft);
  }
  .search input {
    flex: 1;
    min-width: 0;
    height: 100%;
    border: 0;
    background: transparent;
    color: var(--ink);
    font-family: var(--font-mono);
    font-size: var(--fs-sm);
  }
  .search input:focus-visible {
    box-shadow: none;
  }
  .levels {
    display: flex;
    flex-wrap: wrap;
    gap: var(--s-2);
    min-width: 0;
    max-width: 100%;
  }

  .strip {
    display: flex;
    align-items: center;
    gap: var(--s-3);
    padding: var(--s-2) var(--s-2) var(--s-2) var(--s-4);
    border-radius: var(--r-sm);
    font-size: var(--fs-sm);
  }
  .strip p {
    flex: 1;
    min-width: 0;
  }
  .strip.lost {
    background: var(--warn-soft);
    color: var(--ink);
  }
  .strip.lost > :global(.icon) {
    color: var(--warn);
  }
  .strip.failed {
    background: var(--danger-soft);
    color: var(--ink);
  }
  .strip.failed > :global(.icon) {
    color: var(--danger);
  }
  .detail {
    color: var(--ink-2);
  }

  /* The tail: one mono column on the surface, a hairline level tick down each row. */
  .panel {
    position: relative;
    flex: 1;
    min-height: 260px;
    border: 1px solid var(--line);
    border-radius: var(--r-md);
    background: var(--surface);
    box-shadow: var(--e-1);
    overflow: hidden;
  }
  .viewport {
    position: absolute;
    inset: 0;
    overflow: auto;
    overscroll-behavior: contain;
    padding: var(--s-2) 0;
  }
  .viewport:focus-visible {
    box-shadow: inset 0 0 0 2px var(--signal);
    border-radius: var(--r-md);
  }
  .spacer {
    position: relative;
    min-width: max-content;
  }
  .rows {
    position: absolute;
    inset: 0 0 auto;
    will-change: transform;
  }
  .log-row {
    position: relative;
    height: 20px;
    padding: 0 var(--s-4) 0 calc(var(--s-4) + 2px);
    font-family: var(--font-mono);
    font-size: 12.5px;
    font-weight: var(--fw-regular);
    font-variant-ligatures: none;
    line-height: 20px;
    white-space: pre;
    color: var(--ink);
  }
  .log-row::before {
    content: "";
    position: absolute;
    inset: 3px auto 3px var(--s-2);
    width: 2px;
    border-radius: 1px;
    background: var(--line);
  }
  .log-row:hover {
    background: var(--surface-2);
  }
  .log-row:global([hidden]) {
    display: none;
  }
  .log-row:global([data-level="trace"]),
  .log-row:global([data-level="debug"]) {
    color: var(--ink-3);
  }
  .log-row:global([data-level="info"])::before {
    background: var(--signal);
  }
  .log-row:global([data-level="warn"]) {
    color: var(--warn);
  }
  .log-row:global([data-level="warn"])::before {
    background: var(--warn);
  }
  .log-row:global([data-level="error"]) {
    color: var(--danger);
  }
  .log-row:global([data-level="error"])::before {
    background: var(--danger);
  }
  /* Marks are created by `paint`, outside Svelte's scoping. */
  .log-row :global(mark) {
    border-radius: 2px;
    background: var(--warn-soft);
    color: var(--ink);
    box-shadow: 0 0 0 1px color-mix(in oklab, var(--warn) 45%, transparent);
  }
  .empty {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    padding: var(--s-6);
    color: var(--ink-3);
    font-size: var(--fs-sm);
    text-align: center;
  }

  .jump {
    position: absolute;
    right: var(--s-4);
    bottom: var(--s-4);
    display: inline-flex;
    align-items: center;
    gap: var(--s-1);
    min-height: 34px;
    padding: 0 var(--s-3);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-pill);
    background: var(--surface);
    box-shadow: var(--e-2);
    color: var(--ink);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    cursor: pointer;
  }
  .jump :global(.icon) {
    transform: rotate(90deg);
  }
  .jump:focus-visible {
    border-radius: var(--r-pill);
  }

  @media (pointer: coarse) {
    .search {
      min-height: var(--touch);
    }
    .jump {
      min-height: var(--touch);
    }
  }
  @media (max-width: 639px) {
    .logs {
      height: calc(100dvh - var(--tabbar-h) - var(--safe-top) - var(--safe-bottom) - 72px);
      min-height: 360px;
    }
    .levels {
      overflow-x: auto;
      flex-wrap: nowrap;
      width: 100%;
    }
  }
</style>
