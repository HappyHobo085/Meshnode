<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { SvelteSet } from "svelte/reactivity";
  import { api, type MemberDto, type Platform } from "../lib/api";
  import Button from "../lib/design/Button.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import { copyText } from "../lib/device";
  import { hrefOf, navigate } from "../lib/router";
  import { members, status, toast } from "../lib/stores";
  import DeviceCard from "./DeviceCard.svelte";

  let platform = $state<Platform | null>(null);
  let copyFailed = $state(false);

  onMount(() => {
    api
      .platform()
      .then((p) => (platform = p))
      .catch(() => (platform = null));
  });

  /** This device first, then the others online first, then by name. */
  const ordered = $derived(
    [...$members].sort(
      (a, b) =>
        Number(b.isSelf) - Number(a.isSelf) ||
        Number(b.online) - Number(a.online) ||
        a.name.localeCompare(b.name),
    ),
  );

  // Cards slide in only for members that appear while Home is open, not on first paint.
  // `known` is bookkeeping only; `fresh` drives the `arrive` flag.
  let known: ReadonlySet<string> | null = null;
  const fresh = new SvelteSet<string>();
  $effect(() => {
    const ids = $members.map((m) => m.id);
    if (ids.length === 0) return;
    const seen = known;
    known = new Set([...(seen ?? []), ...ids]);
    if (seen === null) return;
    untrack(() => {
      for (const id of ids) if (!seen.has(id)) fresh.add(id);
    });
  });

  const online = $derived($status?.membersOnline ?? $members.filter((m) => m.online).length);
  const total = $derived($status?.membersTotal ?? $members.length);
  const headline = $derived(
    total === 0
      ? "Your devices"
      : `${online} of ${total} ${total === 1 ? "device" : "devices"} online`,
  );
  const gateway = $derived($status?.gatewayUrl ?? null);
  const desktop = $derived(platform !== null && platform !== "android");
  // "Last seen" is measured from the latest poll.
  const now = $derived.by(() => {
    void $members;
    return Date.now();
  });

  async function copyGateway() {
    if (!gateway) return;
    copyFailed = !(await copyText(gateway));
    if (!copyFailed) toast("Gateway URL copied");
  }

  function dotTitle(m: MemberDto): string {
    return `${m.name}: ${m.online ? "online" : "offline"}`;
  }
</script>

<div class="home">
  <section class="hero" aria-labelledby="home-title">
    <div class="hero-main">
      <h1 id="home-title" class="num">{headline}</h1>
      <!-- The mesh at a glance: one bead per device in its own colour, hollow when offline. -->
      {#if ordered.length > 0}
        <div class="strand" aria-hidden="true">
          {#each ordered as m (m.id)}
            <span
              class={["bead", { offline: !m.online, self: m.isSelf }]}
              style:--h={m.accentHue}
              title={dotTitle(m)}
            ></span>
          {/each}
        </div>
      {/if}
    </div>
    <div class="hero-action">
      <Button variant="primary" onclick={() => navigate({ name: "add" })}>
        <Icon name="plus" size={18} />Add device
      </Button>
    </div>
  </section>

  {#if gateway || ($status?.ports.length ?? 0) > 0}
    <section class="gateway" aria-label="Gateway">
      {#if gateway}
        <div class="gw-row">
          <span class="gw-label">Gateway</span>
          <button type="button" class="url" aria-label="Copy gateway URL" onclick={copyGateway}>
            <span class="url-text mono truncate">{gateway}</span>
            <Icon name="copy" size={16} />
          </button>
          {#if desktop}
            <Button
              variant="ghost"
              size="sm"
              aria-label="Open in browser"
              onclick={() => api.desktop.openUrl(gateway)}
            >
              <Icon name="external" size={16} /><span class="open-word">Open</span>
            </Button>
          {/if}
        </div>
        {#if copyFailed}
          <p class="gw-error" role="alert">
            Couldn't copy. Select the address and copy it by hand.
          </p>
        {/if}
      {/if}
      {#if $status && $status.ports.length > 0}
        <ul class="ports wrap" aria-label="Mapped ports">
          {#each $status.ports as p (p.port)}
            <li>
              <span class="port mono num">:{p.port}</span><span class="svc">{p.service}</span>
            </li>
          {/each}
        </ul>
      {/if}
    </section>
  {/if}

  <section class="devices" aria-labelledby="devices-title">
    <h2 id="devices-title" class="visually-hidden">Devices</h2>
    {#if ordered.length === 0}
      <p class="empty">Devices appear here once this one is running.</p>
    {:else}
      <div class="grid">
        {#each ordered as m (m.id)}
          <DeviceCard member={m} arrive={fresh.has(m.id)} {now} />
        {/each}
      </div>
    {/if}
  </section>
</div>

<!-- Phones: the main action floats above the tab bar. -->
<a class="fab" href={hrefOf({ name: "add" })} aria-label="Add device">
  <Icon name="plus" size={24} />
</a>

<style>
  .home {
    display: grid;
    gap: var(--s-6);
  }

  .hero {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: var(--s-4) var(--s-6);
    flex-wrap: wrap;
  }
  .hero-main {
    display: grid;
    gap: var(--s-3);
    min-width: 0;
  }
  h1 {
    font-size: var(--fs-3xl);
    font-weight: var(--fw-bold);
    letter-spacing: -0.03em;
  }

  /* The strand: devices as beads on one thread. */
  .strand {
    position: relative;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px 10px;
    padding: 2px 0;
  }
  .bead {
    position: relative;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--accent);
    box-shadow: 0 0 0 3px var(--bg);
  }
  .bead:not(:last-child)::after {
    content: "";
    position: absolute;
    top: 50%;
    left: 100%;
    width: 10px;
    height: 1px;
    background: var(--line-strong);
  }
  .bead.self {
    width: 16px;
    height: 16px;
    box-shadow:
      0 0 0 3px var(--bg),
      0 0 0 4px color-mix(in oklab, var(--accent) 60%, transparent);
  }
  .bead.offline {
    background: transparent;
    box-shadow:
      0 0 0 3px var(--bg),
      inset 0 0 0 2px color-mix(in oklab, var(--accent) 55%, var(--ink-3));
  }

  .gateway {
    display: grid;
    gap: var(--s-3);
    padding: var(--s-3) var(--s-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--r-md);
  }
  .gw-row {
    display: flex;
    align-items: center;
    gap: var(--s-3);
    min-width: 0;
  }
  .gw-label {
    flex: none;
    font-size: var(--fs-sm);
    color: var(--ink-3);
  }
  .url {
    display: inline-flex;
    align-items: center;
    gap: var(--s-2);
    min-width: 0;
    min-height: 36px;
    padding: 0 var(--s-3);
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    background: var(--surface-2);
    color: var(--ink);
    cursor: copy;
  }
  .url:hover {
    border-color: var(--line-strong);
  }
  .url :global(.icon) {
    color: var(--ink-3);
  }
  .url-text {
    font-size: var(--fs-sm);
  }
  .gw-error {
    font-size: var(--fs-sm);
    color: var(--danger);
  }
  .ports {
    gap: var(--s-2);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .ports li {
    display: inline-flex;
    align-items: baseline;
    gap: var(--s-2);
    min-width: 0;
    padding: 2px var(--s-3);
    border-radius: var(--r-pill);
    background: var(--surface-2);
    font-size: var(--fs-sm);
  }
  .port {
    color: var(--ink-2);
  }
  .svc {
    color: var(--ink);
    font-weight: var(--fw-medium);
    overflow-wrap: anywhere;
  }

  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(min(100%, 300px), 1fr));
    gap: var(--s-4);
  }
  .empty {
    color: var(--ink-3);
  }

  .fab {
    display: none;
  }

  @media (pointer: coarse) {
    .url {
      min-height: var(--touch);
    }
  }

  @media (max-width: 639px) {
    h1 {
      font-size: var(--fs-2xl);
    }
    .hero-action {
      display: none;
    }
    .open-word {
      display: none;
    }
    /* The address matters more than its label on a narrow screen. */
    .gw-row {
      flex-wrap: wrap;
      row-gap: var(--s-1);
    }
    .gw-label {
      width: 100%;
    }
    .fab {
      position: fixed;
      right: calc(var(--s-4) + var(--safe-right));
      bottom: calc(var(--tabbar-h) + var(--s-4) + var(--safe-bottom));
      z-index: 30;
      display: grid;
      place-items: center;
      width: 56px;
      height: 56px;
      border-radius: var(--r-lg);
      background: var(--signal);
      color: var(--signal-ink);
      box-shadow: var(--e-3);
    }
    .fab:focus-visible {
      border-radius: var(--r-lg);
    }
  }
</style>
