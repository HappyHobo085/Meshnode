<script lang="ts">
  import type { MemberDto } from "../lib/api";
  import Card from "../lib/design/Card.svelte";
  import Chip from "../lib/design/Chip.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import Monogram from "../lib/design/Monogram.svelte";
  import { platformOf } from "../lib/device";
  import { relativeTime, rtt } from "../lib/format";

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
    {#if member.isSelf}<span class="self">This device</span>{/if}
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
  }
</style>
