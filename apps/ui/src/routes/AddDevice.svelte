<script lang="ts">
  import { untrack } from "svelte";
  import {
    api,
    toUiError,
    type InviteDto,
    type MemberDto,
    type RoleDto,
    type UiError,
  } from "../lib/api";
  import Button from "../lib/design/Button.svelte";
  import Card from "../lib/design/Card.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import Ring from "../lib/design/Ring.svelte";
  import { copyText } from "../lib/device";
  import { clock } from "../lib/format";
  import { parseQr } from "../lib/qr";
  import { hrefOf, navigate } from "../lib/router";
  import { members, toast } from "../lib/stores";
  import DeviceCard from "./DeviceCard.svelte";

  interface Props {
    /** The clock (tests pass a fake one). */
    now?: () => number;
  }
  let { now = () => Date.now() }: Props = $props();

  const ROLES: { value: RoleDto; label: string; help: string }[] = [
    { value: "host", label: "Host", help: "Runs your services and keeps a full copy of the mesh." },
    {
      value: "client",
      label: "Client",
      help: "Reaches your services through the mesh, without running them.",
    },
  ];
  /** Invite lifetimes the daemon accepts, in minutes (ttl_s 300, 900, 3600). */
  const EXPIRY_MIN = [5, 15, 60] as const;

  let role = $state<RoleDto>("host");
  let minutes = $state<(typeof EXPIRY_MIN)[number]>(15);

  let invite = $state<InviteDto | null>(null);
  let ttlMs = $state(0);
  let nowMs = $state(0);
  let busy = $state(false);
  let error = $state<UiError | null>(null);
  /** Member ids present when the invite was made; anyone else has joined through it. */
  let before: Set<string> | null = null;
  let joined = $state<MemberDto | null>(null);

  const qr = $derived(invite ? parseQr(invite.qrSvg) : null);
  const left = $derived(invite ? invite.expiresAtMs - nowMs : 0);
  const phase = $derived(joined ? "joined" : !invite ? "setup" : left <= 0 ? "expired" : "waiting");

  async function create() {
    busy = true;
    error = null;
    try {
      const ids = $members.map((m) => m.id);
      const fresh = await api.invite(role, minutes * 60);
      before = ids.length > 0 ? new Set(ids) : null;
      joined = null;
      ttlMs = minutes * 60_000;
      nowMs = now();
      invite = fresh;
    } catch (e) {
      error = toUiError(e);
    } finally {
      busy = false;
    }
  }

  function reset() {
    invite = null;
    joined = null;
    before = null;
  }

  // The countdown ticks once a second while the invite is live.
  $effect(() => {
    if (phase !== "waiting") return;
    const id = setInterval(() => (nowMs = now()), 1000);
    return () => clearInterval(id);
  });

  // Success: a member id that wasn't there when the invite was made (seen through the poll).
  $effect(() => {
    const list = $members;
    untrack(() => {
      if (!invite || joined) return;
      if (before === null) {
        // Members weren't loaded yet when the invite was made: start from this list.
        if (list.length > 0) before = new Set(list.map((m) => m.id));
        return;
      }
      const base = before;
      joined = list.find((m) => !base.has(m.id)) ?? null;
    });
  });

  async function copyTicket() {
    if (invite && (await copyText(invite.text))) toast("Invite copied");
  }
</script>

<div class="add">
  <header class="top">
    <a class="back" href={hrefOf({ name: "home" })}><Icon name="back" size={18} />Home</a>
    <h1>Add a device</h1>
  </header>

  {#if phase === "setup"}
    <Card padding="lg" class="setup">
      <form
        onsubmit={(e) => {
          e.preventDefault();
          void create();
        }}
      >
        <fieldset>
          <legend>What will the new device do?</legend>
          <div class="roles">
            {#each ROLES as r (r.value)}
              <label class={["role", { on: role === r.value }]}>
                <input
                  type="radio"
                  name="role"
                  value={r.value}
                  bind:group={role}
                  aria-describedby="role-{r.value}-help"
                />
                <span class="role-text">
                  <span class="role-name">{r.label}</span>
                  <span class="role-help" id="role-{r.value}-help">{r.help}</span>
                </span>
              </label>
            {/each}
          </div>
        </fieldset>

        <fieldset>
          <legend>Invite expires after</legend>
          <div class="segments">
            {#each EXPIRY_MIN as m (m)}
              <label class={["segment", { on: minutes === m }]}>
                <input type="radio" name="expiry" value={m} bind:group={minutes} />
                <span>{m} min</span>
              </label>
            {/each}
          </div>
        </fieldset>

        {#if error}
          <p class="error" role="alert">
            {error.message}{#if error.hint}<span class="hint">{error.hint}</span>{/if}
          </p>
        {/if}

        <div class="actions">
          <Button variant="primary" type="submit" {busy}>Create invite</Button>
        </div>
      </form>
    </Card>
  {:else if phase === "waiting" && invite}
    <Card padding="lg" class="invite">
      <div class="qr-tile">
        {#if qr}
          <svg
            class="qr"
            viewBox={qr.viewBox}
            shape-rendering="crispEdges"
            role="img"
            aria-label="Invite QR code"
          >
            <path d={qr.d} />
          </svg>
        {:else}
          <p class="no-qr">This invite's QR code couldn't be drawn. Copy the text instead.</p>
        {/if}
      </div>

      <div class="side">
        <div class="wait">
          <Ring
            fraction={left / ttlMs}
            size={108}
            stroke={5}
            label="Invite expires in {clock(left)}"
          >
            <span class="clock num">{clock(left)}</span>
          </Ring>
          <div class="wait-text">
            <h2>Scan this on the new device</h2>
            <p>
              In Meshnode on that device, choose <strong>Join a mesh</strong>. This page updates by
              itself when it joins.
            </p>
          </div>
        </div>

        <div class="ticket">
          <span class="ticket-label">Or paste this invite</span>
          <div class="ticket-box">
            <code class="ticket-text">{invite.text}</code>
            <Button size="sm" aria-label="Copy invite" onclick={copyTicket}>
              <Icon name="copy" size={16} />Copy
            </Button>
          </div>
          <p class="ticket-note">It works once, for one device. Anyone holding it can join.</p>
        </div>

        <div class="actions">
          <Button variant="ghost" onclick={reset}>Cancel</Button>
        </div>
      </div>
    </Card>
  {:else if phase === "expired"}
    <Card padding="lg" class="ended">
      <span class="ended-icon"><Icon name="refresh" size={22} /></span>
      <h2>Invite expired</h2>
      <p>No device joined in time. A new invite starts the countdown again.</p>
      {#if error}
        <p class="error" role="alert">
          {error.message}{#if error.hint}<span class="hint">{error.hint}</span>{/if}
        </p>
      {/if}
      <div class="actions">
        <Button variant="ghost" onclick={reset}>Change options</Button>
        <Button variant="primary" {busy} onclick={create}>New invite</Button>
      </div>
    </Card>
  {:else if phase === "joined" && joined}
    <section class="joined" aria-live="polite">
      <div class="joined-head" style:--h={joined.accentHue}>
        <span class="tick"><Icon name="check" size={22} /></span>
        <h2>{joined.name} joined</h2>
      </div>
      <div class="joined-card">
        <DeviceCard member={joined} arrive />
      </div>
      <div class="actions">
        <Button variant="ghost" onclick={reset}>Add another</Button>
        <Button variant="primary" onclick={() => navigate({ name: "home" })}>Done</Button>
      </div>
    </section>
  {/if}
</div>

<style>
  .add {
    display: grid;
    gap: var(--s-6);
    max-width: 820px;
  }
  .top {
    display: grid;
    gap: var(--s-2);
  }
  .back {
    display: inline-flex;
    align-items: center;
    gap: var(--s-1);
    justify-self: start;
    min-height: 32px;
    color: var(--ink-2);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    text-decoration: none;
  }
  .back:hover {
    color: var(--ink);
  }
  @media (pointer: coarse) {
    .back {
      min-height: var(--touch);
    }
  }

  form {
    display: grid;
    gap: var(--s-6);
  }
  fieldset {
    display: grid;
    gap: var(--s-3);
    min-width: 0;
    margin: 0;
    padding: 0;
    border: 0;
  }
  legend {
    padding: 0;
    margin-bottom: var(--s-3);
    font-weight: var(--fw-semibold);
  }

  /* Role: two choice tiles. The native radio stays (keyboard, screen readers), drawn small. */
  .roles {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(100%, 220px), 1fr));
    gap: var(--s-3);
  }
  .role {
    display: flex;
    align-items: flex-start;
    gap: var(--s-3);
    padding: var(--s-4);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-md);
    cursor: pointer;
    transition:
      border-color var(--dur-1) var(--ease-out),
      background-color var(--dur-1) var(--ease-out);
  }
  .role:hover {
    border-color: var(--ink-3);
  }
  .role.on {
    border-color: var(--signal);
    background: var(--signal-soft);
  }
  .role input {
    margin: 3px 0 0;
    accent-color: var(--signal);
  }
  .role-text {
    display: grid;
    gap: 2px;
  }
  .role-name {
    font-weight: var(--fw-semibold);
  }
  .role-help {
    font-size: var(--fs-sm);
    color: var(--ink-2);
  }

  /* Expiry: a segmented control built from radios. */
  .segments {
    display: inline-flex;
    justify-self: start;
    padding: 3px;
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    background: var(--surface-2);
  }
  .segment {
    position: relative;
    display: grid;
    place-items: center;
    min-width: 76px;
    min-height: 36px;
    padding: 0 var(--s-3);
    border-radius: 6px;
    color: var(--ink-2);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    cursor: pointer;
  }
  .segment input {
    position: absolute;
    opacity: 0;
    pointer-events: none;
  }
  .segment.on {
    background: var(--surface);
    color: var(--ink);
    box-shadow: var(--e-1);
  }
  .segment:has(input:focus-visible) {
    box-shadow: var(--focus-ring);
  }
  @media (pointer: coarse) {
    .segment {
      min-height: var(--touch);
    }
  }

  .actions {
    display: flex;
    flex-wrap: wrap;
    justify-content: flex-end;
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

  /* The live invite: the QR on its own white tile (scanners want dark-on-light in both themes). */
  :global(.card.invite) {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    gap: var(--s-8);
    align-items: start;
  }
  .qr-tile {
    display: grid;
    place-items: center;
    width: 264px;
    aspect-ratio: 1;
    padding: var(--s-3);
    border-radius: var(--r-md);
    background: #fff;
    box-shadow:
      0 0 0 1px var(--line),
      var(--e-2);
  }
  .qr {
    width: 100%;
    height: 100%;
    fill: #111;
  }
  .no-qr {
    padding: var(--s-4);
    color: #444;
    font-size: var(--fs-sm);
    text-align: center;
  }
  .side {
    display: grid;
    gap: var(--s-6);
    min-width: 0;
  }
  .wait {
    display: flex;
    align-items: center;
    gap: var(--s-5);
  }
  .clock {
    font-size: var(--fs-lg);
    font-weight: var(--fw-semibold);
    letter-spacing: -0.01em;
  }
  .wait-text {
    display: grid;
    gap: var(--s-1);
    min-width: 0;
  }
  .wait-text h2 {
    font-size: var(--fs-lg);
  }
  .wait-text p {
    font-size: var(--fs-sm);
    color: var(--ink-2);
  }
  .ticket {
    display: grid;
    gap: var(--s-2);
  }
  .ticket-label {
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    color: var(--ink-2);
  }
  .ticket-box {
    display: flex;
    align-items: center;
    gap: var(--s-3);
    padding: var(--s-2) var(--s-2) var(--s-2) var(--s-3);
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    background: var(--surface-2);
  }
  .ticket-text {
    flex: 1;
    min-width: 0;
    max-height: 4.6em;
    overflow-y: auto;
    font-size: var(--fs-xs);
    line-height: 1.5;
    color: var(--ink);
    overflow-wrap: anywhere;
    user-select: all;
  }
  .ticket-note {
    font-size: var(--fs-xs);
    color: var(--ink-3);
  }

  :global(.card.ended) {
    display: grid;
    justify-items: start;
    gap: var(--s-3);
  }
  .ended-icon {
    display: grid;
    place-items: center;
    width: 44px;
    height: 44px;
    border-radius: var(--r-md);
    background: var(--warn-soft);
    color: var(--warn);
  }
  :global(.card.ended) p {
    color: var(--ink-2);
  }
  :global(.card.ended) .actions {
    justify-self: stretch;
    margin-top: var(--s-3);
  }

  .joined {
    display: grid;
    gap: var(--s-5);
    max-width: 460px;
  }
  .joined-head {
    display: flex;
    align-items: center;
    gap: var(--s-3);
    min-width: 0;
  }
  .joined-head h2 {
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .tick {
    display: grid;
    flex: none;
    place-items: center;
    width: 40px;
    height: 40px;
    border-radius: 50%;
    background: var(--ok);
    color: var(--surface);
    animation: mesh-arrive var(--dur-3) var(--ease-out) both;
  }

  @media (max-width: 719px) {
    :global(.card.invite) {
      grid-template-columns: minmax(0, 1fr);
      justify-items: center;
      gap: var(--s-6);
    }
    .qr-tile {
      width: min(100%, 280px);
    }
    .side {
      width: 100%;
    }
  }
  @media (max-width: 639px) {
    .actions > :global(*) {
      flex: 1;
    }
  }
</style>
