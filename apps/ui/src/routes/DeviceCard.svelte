<script lang="ts">
  import { tick } from "svelte";
  import { api, type MemberDto } from "../lib/api";
  import Card from "../lib/design/Card.svelte";
  import Chip from "../lib/design/Chip.svelte";
  import Confirm from "../lib/design/Confirm.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import Monogram from "../lib/design/Monogram.svelte";
  import { platformOf } from "../lib/device";
  import { relativeTime, rtt } from "../lib/format";
  import { refreshNow, toast } from "../lib/stores";

  interface Props {
    member: MemberDto;
    /** Slides in: the member has just appeared. */
    arrive?: boolean;
    /** The time "last seen" is measured from. */
    now?: number;
  }
  let { member, arrive = false, now = Date.now() }: Props = $props();

  /** Tags shown before the rest fold behind "+n". */
  const FOLD = 5;
  let expanded = $state(false);

  const platform = $derived(platformOf(member.platform));
  const shownTags = $derived(expanded ? member.tags : member.tags.slice(0, FOLD));
  const hidden = $derived(member.tags.length - shownTags.length);
  const seen = $derived(
    member.online
      ? null
      : member.lastSeenMs === null
        ? "not seen yet"
        : `last seen ${relativeTime(member.lastSeenMs, now)}`,
  );
  const sync = $derived(member.lag === 0 ? "Up to date" : `${member.lag.toLocaleString()} behind`);

  // The card menu (other devices only): Remove device…
  const uid = $props.id();
  let menuOpen = $state(false);
  let menuButton: HTMLButtonElement | undefined = $state();
  let menu: HTMLElement | undefined = $state();
  let confirmRemove = $state(false);

  async function openMenu() {
    menuOpen = true;
    await tick();
    menu?.querySelector<HTMLElement>('[role="menuitem"]')?.focus();
  }

  function closeMenu(refocus = true) {
    menuOpen = false;
    if (refocus) menuButton?.focus();
  }

  function onMenuKey(e: KeyboardEvent) {
    if (e.key === "Escape" || e.key === "Tab") {
      if (e.key === "Escape") e.preventDefault();
      closeMenu(e.key === "Escape");
    }
  }

  // A press anywhere outside the menu closes it.
  $effect(() => {
    if (!menuOpen) return;
    const onDown = (e: PointerEvent) => {
      const t = e.target as Node;
      if (!menu?.contains(t) && !menuButton?.contains(t)) closeMenu(false);
    };
    document.addEventListener("pointerdown", onDown);
    return () => document.removeEventListener("pointerdown", onDown);
  });

  function askRemove() {
    closeMenu(false);
    confirmRemove = true;
  }

  async function remove() {
    const name = member.name;
    await api.remove(member.id);
    confirmRemove = false;
    await refreshNow();
    toast(`Removed ${name}`);
  }
</script>

<Card hue={member.accentHue} {arrive} muted={!member.online} class="device">
  <div class="head">
    <Monogram name={member.name} hue={member.accentHue} online={member.online} />
    <div class="who">
      <h3 class="name truncate" title={member.name}>{member.name}</h3>
      <p class={["state", { online: member.online }]}>
        <span class="state-word">{member.online ? "Online" : "Offline"}{seen ? "," : ""}</span>
        {#if seen}<span class="seen">{seen}</span>{/if}
      </p>
    </div>
    {#if member.isSelf}
      <span class="self">This device</span>
    {:else}
      <div class="menu-wrap">
        <button
          type="button"
          class="menu-btn"
          bind:this={menuButton}
          aria-label="More actions for {member.name}"
          aria-haspopup="menu"
          aria-expanded={menuOpen}
          aria-controls={menuOpen ? `${uid}-menu` : undefined}
          onclick={() => (menuOpen ? closeMenu() : openMenu())}
        >
          <Icon name="more" size={18} />
        </button>
        {#if menuOpen}
          <div
            class="menu"
            id="{uid}-menu"
            role="menu"
            tabindex="-1"
            aria-label="Actions for {member.name}"
            bind:this={menu}
            onkeydown={onMenuKey}
          >
            <button type="button" class="menu-item danger" role="menuitem" onclick={askRemove}>
              <Icon name="trash" size={16} />Remove device…
            </button>
          </div>
        {/if}
      </div>
    {/if}
  </div>

  <dl class="facts">
    <div>
      <dt>Device</dt>
      <dd><Icon name={platform.icon} size={15} /><span class="truncate">{platform.label}</span></dd>
    </div>
    <div>
      <dt>Role</dt>
      <dd>{member.role === "host" ? "Host" : "Client"}{member.effective ? "" : " (pending)"}</dd>
    </div>
    <div>
      <dt>Latency</dt>
      <dd class="num">{member.isSelf ? "—" : rtt(member.rttMs)}</dd>
    </div>
    <div>
      <dt>Sync</dt>
      <dd class={["num", { behind: member.lag > 0 }]}>{sync}</dd>
    </div>
  </dl>

  {#if member.tags.length > 0}
    <div class="tags wrap" aria-label="Tags">
      {#each shownTags as tag (tag)}
        <Chip label={tag} hue={member.accentHue} />
      {/each}
      {#if hidden > 0 || expanded}
        <button
          type="button"
          class="more"
          aria-expanded={expanded}
          aria-label={expanded ? "Show fewer tags" : `Show ${hidden} more tags`}
          onclick={() => (expanded = !expanded)}
        >
          {expanded ? "Less" : `+${hidden}`}
        </button>
      {/if}
    </div>
  {/if}
</Card>

{#if !member.isSelf}
  <Confirm
    open={confirmRemove}
    title="Remove {member.name}?"
    consequence="{member.name} leaves your mesh at once and stops syncing. This can't be undone: a removed device can never rejoin with the same identity. To bring it back, reset it on that device and invite it again."
    typeToConfirm={member.name}
    actionLabel="Remove device"
    onConfirm={remove}
    onCancel={() => (confirmRemove = false)}
  />
{/if}

<style>
  :global(.card.device) {
    display: grid;
    gap: var(--s-4);
    align-content: start;
    min-width: 0;
  }
  .head {
    display: flex;
    align-items: center;
    gap: var(--s-3);
    min-width: 0;
  }
  .who {
    flex: 1;
    min-width: 0;
  }
  .name {
    font-size: var(--fs-md);
    font-weight: var(--fw-semibold);
    letter-spacing: -0.005em;
    line-height: var(--lh-snug);
  }
  .state {
    display: flex;
    flex-wrap: wrap;
    column-gap: 0.3em;
    font-size: var(--fs-sm);
    color: var(--ink-3);
  }
  .state.online .state-word {
    color: var(--ok);
    font-weight: var(--fw-medium);
  }
  .self {
    flex: none;
    align-self: start;
    padding: 2px var(--s-2);
    border-radius: var(--r-pill);
    background: var(--signal-soft);
    color: var(--signal);
    font-size: var(--fs-xs);
    font-weight: var(--fw-semibold);
    white-space: nowrap;
  }

  /* The card menu: a quiet "more" button, a small popover with the one action. */
  .menu-wrap {
    position: relative;
    flex: none;
    align-self: start;
    margin: calc(var(--s-1) * -1) calc(var(--s-2) * -1) 0 0;
  }
  .menu-btn {
    display: grid;
    place-items: center;
    width: 32px;
    height: 32px;
    border: 0;
    border-radius: var(--r-sm);
    background: transparent;
    color: var(--ink-3);
    cursor: pointer;
  }
  .menu-btn:hover,
  .menu-btn[aria-expanded="true"] {
    background: var(--surface-2);
    color: var(--ink);
  }
  .menu {
    position: absolute;
    top: calc(100% + var(--s-1));
    right: 0;
    z-index: 20;
    display: grid;
    min-width: 190px;
    padding: var(--s-1);
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    background: var(--surface);
    box-shadow: var(--e-3);
    animation: mesh-arrive var(--dur-1) var(--ease-out) both;
  }
  .menu-item {
    display: flex;
    align-items: center;
    gap: var(--s-2);
    min-height: 36px;
    padding: 0 var(--s-3);
    border: 0;
    border-radius: var(--r-xs);
    background: transparent;
    color: var(--ink);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    text-align: left;
    white-space: nowrap;
    cursor: pointer;
  }
  .menu-item.danger {
    color: var(--danger);
  }
  .menu-item:hover,
  .menu-item:focus-visible {
    background: var(--surface-2);
  }

  /* Four small facts, two by two: easy to scan across many cards. */
  .facts {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: var(--s-3) var(--s-4);
    margin: 0;
    padding-top: var(--s-3);
    border-top: 1px solid var(--line);
  }
  .facts > div {
    min-width: 0;
  }
  dt {
    font-size: var(--fs-xs);
    color: var(--ink-3);
  }
  dd {
    display: flex;
    align-items: center;
    gap: var(--s-1);
    min-width: 0;
    margin: 0;
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    color: var(--ink);
  }
  dd :global(.icon) {
    color: var(--ink-3);
  }
  .behind {
    color: var(--warn);
  }

  .tags {
    gap: var(--s-1) var(--s-2);
  }
  .tags :global(.chip) {
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .more {
    height: 28px;
    padding: 0 var(--s-3);
    border: 1px dashed var(--line-strong);
    border-radius: var(--r-pill);
    background: transparent;
    color: var(--ink-2);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    cursor: pointer;
  }
  .more:hover {
    color: var(--ink);
    border-style: solid;
  }
  .more:focus-visible {
    border-radius: var(--r-pill);
  }
  @media (pointer: coarse) {
    .more {
      height: var(--touch);
    }
    .menu-btn {
      width: var(--touch);
      height: var(--touch);
    }
    .menu-item {
      min-height: var(--touch);
    }
  }
</style>
