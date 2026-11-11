<script lang="ts">
  import Button from "../lib/design/Button.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import Monogram from "../lib/design/Monogram.svelte";
  import { bytes } from "../lib/format";
  import { hrefOf, navigate } from "../lib/router";
  import { hostsOf, kindOf, listNames, localState } from "../lib/service";
  import { members, services } from "../lib/stores";
  import Deploy from "./Deploy.svelte";
  import ServiceDetail from "./ServiceDetail.svelte";

  interface Props {
    /** The service whose detail sheet is open (`#/services/:name`). */
    detail?: string | null;
    /** The Deploy sheet is open (`#/deploy`). */
    deploying?: boolean;
  }
  let { detail = null, deploying = false }: Props = $props();

  /** Hosts drawn as tiles before the rest fold into "+n". */
  const SHOWN = 4;

  // Keep the last name while the sheet animates out.
  let sheetName = $state("");
  $effect(() => {
    if (detail !== null) sheetName = detail;
  });

  const rows = $derived(
    [...$services]
      .sort((a, b) => a.name.localeCompare(b.name))
      .map((s) => {
        const hosts = hostsOf(s, $members);
        return {
          s,
          kind: kindOf(s.kind),
          here: localState(s),
          hosts,
          runsOn: hosts.length > 0 ? `Runs on ${listNames(hosts.map((h) => h.name))}` : "No host",
        };
      }),
  );

  const close = () => navigate({ name: "services" });
</script>

<div class="services">
  <header class="top">
    <div>
      <h1>Services</h1>
      {#if rows.length > 0}
        <p class="sub num">{rows.length} deployed across your hosts</p>
      {/if}
    </div>
    <Button variant="primary" onclick={() => navigate({ name: "deploy" })}>
      <Icon name="upload" size={18} />Deploy
    </Button>
  </header>

  {#if rows.length === 0}
    <section class="empty">
      <span class="empty-icon"><Icon name="services" size={26} /></span>
      <h2>No services yet</h2>
      <p>
        Deploy a site folder or a .wasm module. Meshnode runs it on the hosts you choose and keeps
        every copy in step.
      </p>
    </section>
  {:else}
    <ul class="list">
      {#each rows as r (r.s.name)}
        <li>
          <a class="row" href={hrefOf({ name: "service", params: { name: r.s.name } })}>
            <span class="kind" title={r.kind.label}><Icon name={r.kind.icon} size={20} /></span>

            <span class="ident">
              <span class="name truncate">{r.s.name}</span>
              <span class="meta">
                <span>{r.kind.label}</span>
                <span class="ver mono num">v{r.s.version}</span>
              </span>
            </span>

            <span class="hosts" title={r.runsOn}>
              <span class="stack" aria-hidden="true">
                {#each r.hosts.slice(0, SHOWN) as h (h.id)}
                  <Monogram name={h.name} hue={h.hue} size="sm" />
                {/each}
                {#if r.hosts.length > SHOWN}
                  <span class="more num">+{r.hosts.length - SHOWN}</span>
                {/if}
                {#if r.hosts.length === 0}<span class="nobody"></span>{/if}
              </span>
              <span class={["runs truncate", { warn: r.hosts.length === 0 }]}>{r.runsOn}</span>
            </span>

            <span class="here">
              <span class={["pill", r.here.tone]}>{r.here.text}</span>
              {#if r.here.detail}
                <span class="failure truncate" title={r.here.detail}>{r.here.detail}</span>
              {/if}
            </span>

            <span class="size">
              <span class="num">{bytes(r.s.dataBytes)}</span>
              <span class="size-label">data</span>
            </span>

            <span class="chev"><Icon name="chevron" size={18} /></span>
          </a>
        </li>
      {/each}
    </ul>
  {/if}
</div>

<ServiceDetail open={detail !== null} name={detail ?? sheetName} onclose={close} />
<Deploy open={deploying} onclose={close} />

<style>
  .services {
    display: grid;
    gap: var(--s-6);
  }
  .top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--s-4);
  }
  .sub {
    margin-top: var(--s-1);
    color: var(--ink-3);
    font-size: var(--fs-sm);
  }

  .empty {
    display: grid;
    justify-items: start;
    gap: var(--s-3);
    max-width: 520px;
    padding: var(--s-8) 0;
  }
  .empty-icon {
    display: grid;
    place-items: center;
    width: 52px;
    height: 52px;
    border-radius: var(--r-md);
    background: var(--signal-soft);
    color: var(--signal);
  }
  .empty h2 {
    font-size: var(--fs-xl);
  }
  .empty p {
    color: var(--ink-2);
  }

  /* One list, hairline-separated rows: services read as a ledger, not a wall of cards. */
  .list {
    margin: 0;
    padding: 0;
    list-style: none;
    border: 1px solid var(--line);
    border-radius: var(--r-md);
    background: var(--surface);
    box-shadow: var(--e-1);
    overflow: hidden;
  }
  .list li + li {
    border-top: 1px solid var(--line);
  }
  .row {
    display: grid;
    grid-template-columns:
      40px minmax(0, 1.2fr) minmax(0, 1.3fr) minmax(0, 1fr)
      72px 18px;
    align-items: center;
    gap: var(--s-4);
    min-height: 72px;
    padding: var(--s-3) var(--s-4) var(--s-3) var(--s-4);
    color: var(--ink);
    text-decoration: none;
    transition: background-color var(--dur-1) var(--ease-out);
  }
  .row:hover {
    background: color-mix(in oklab, var(--surface-2) 60%, var(--surface));
  }
  .row:focus-visible {
    border-radius: 0;
    box-shadow: inset 0 0 0 2px var(--signal);
  }
  @media (pointer: coarse) {
    .row {
      min-height: var(--touch);
    }
  }

  .kind {
    display: grid;
    place-items: center;
    width: 40px;
    height: 40px;
    border-radius: var(--r-sm);
    background: var(--surface-2);
    color: var(--ink-2);
  }
  .ident {
    display: grid;
    min-width: 0;
  }
  .name {
    font-weight: var(--fw-semibold);
    letter-spacing: -0.005em;
  }
  .meta {
    display: flex;
    align-items: center;
    gap: var(--s-2);
    color: var(--ink-3);
    font-size: var(--fs-sm);
  }
  .ver {
    padding: 0 var(--s-1);
    border-radius: var(--r-xs);
    background: var(--surface-2);
    color: var(--ink-2);
    font-size: var(--fs-xs);
  }

  /* Who runs it: the hosts' own tiles, overlapping like a hand of cards. */
  .hosts {
    display: flex;
    align-items: center;
    gap: var(--s-3);
    min-width: 0;
  }
  .stack {
    display: flex;
    flex: none;
    padding-left: 4px;
  }
  /* Opaque tiles (the soft hue laid over the surface) so overlaps stay clean. */
  .stack > :global(*) {
    margin-left: -4px;
  }
  .stack > :global(.mono-tile),
  .stack > .more {
    box-shadow:
      0 0 0 2px var(--surface),
      inset 0 0 0 1px color-mix(in oklab, var(--accent) 45%, transparent);
  }
  .stack > :global(.mono-tile) {
    background: linear-gradient(var(--accent-soft), var(--accent-soft)), var(--surface);
  }
  .more,
  .nobody {
    display: grid;
    place-items: center;
    width: 28px;
    height: 28px;
    border-radius: 9px;
    background: var(--surface-2);
    color: var(--ink-2);
    font-size: var(--fs-xs);
    font-weight: var(--fw-semibold);
  }
  .nobody {
    background: transparent;
    border: 1.5px dashed var(--line-strong);
    box-shadow: none;
  }
  .runs {
    color: var(--ink-2);
    font-size: var(--fs-sm);
  }
  .runs.warn {
    color: var(--warn);
  }

  .here {
    display: grid;
    justify-items: start;
    gap: 2px;
    min-width: 0;
  }
  .pill {
    max-width: 100%;
    padding: 2px var(--s-2);
    border-radius: var(--r-pill);
    font-size: var(--fs-xs);
    font-weight: var(--fw-semibold);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .pill.ok {
    background: var(--ok-soft);
    color: var(--ok);
  }
  .pill.warn {
    background: var(--warn-soft);
    color: var(--warn);
  }
  .pill.danger {
    background: var(--danger-soft);
    color: var(--danger);
  }
  .pill.quiet {
    padding-left: 0;
    color: var(--ink-3);
  }
  .failure {
    max-width: 100%;
    color: var(--danger);
    font-size: var(--fs-xs);
  }

  .size {
    display: grid;
    justify-items: end;
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
  }
  .size-label {
    color: var(--ink-3);
    font-size: var(--fs-xs);
    font-weight: var(--fw-regular);
  }
  .chev {
    color: var(--ink-3);
  }

  /* Narrow windows: the state moves under the name, the hosts below. */
  @media (max-width: 899px) {
    .row {
      grid-template-columns: 40px minmax(0, 1fr) auto 18px;
      grid-template-areas:
        "kind ident size chev"
        "kind hosts here chev";
      row-gap: var(--s-2);
    }
    .kind {
      grid-area: kind;
      align-self: start;
    }
    .ident {
      grid-area: ident;
    }
    .hosts {
      grid-area: hosts;
    }
    .here {
      grid-area: here;
      justify-items: end;
    }
    .size {
      grid-area: size;
    }
    .chev {
      grid-area: chev;
    }
  }
  @media (max-width: 479px) {
    .row {
      grid-template-columns: 40px minmax(0, 1fr) auto;
      grid-template-areas:
        "kind ident size"
        "kind hosts hosts"
        "kind here here";
      padding: var(--s-3);
    }
    .here {
      justify-items: start;
    }
    .chev {
      display: none;
    }
  }
</style>
