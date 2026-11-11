<script lang="ts">
  import { onMount } from "svelte";
  import { api, toUiError, type Platform, type UiError } from "../lib/api";
  import Button from "../lib/design/Button.svelte";
  import Card from "../lib/design/Card.svelte";
  import Field from "../lib/design/Field.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import { chosenName, refusalText, suggestedName } from "../lib/device";
  import { hrefOf, navigate } from "../lib/router";
  import { refreshNow, status, toast } from "../lib/stores";

  let platform = $state<Platform | null>(null);
  let loaded = $state(false);
  let name = $state($chosenName ?? "");
  let ticket = $state("");
  let pasting = $state(false);
  let busy = $state(false);
  let scanning = $state(false);
  let error = $state<UiError | null>(null);

  const android = $derived(platform === "android");
  const showPaste = $derived(loaded && (!android || pasting));
  const back = $derived(
    $status?.state === "uninitialised" ? hrefOf({ name: "first-run" }) : hrefOf({ name: "home" }),
  );

  onMount(() => {
    api
      .platform()
      .then((p) => (platform = p))
      .catch(() => (platform = null))
      .finally(async () => {
        loaded = true;
        const suggested = await suggestedName(platform);
        if (name === "") name = suggested;
      });
  });

  async function join(text: string) {
    const t = text.trim();
    const n = name.trim();
    if (!t || !n) return;
    busy = true;
    error = null;
    try {
      await api.join(t, n);
      chosenName.set(null);
      toast("Joined your mesh");
      // Wait for the new state, or App would still see "uninitialised" and send us back.
      await refreshNow();
      navigate({ name: "home" }, { replace: true });
    } catch (e) {
      error = toUiError(e);
    } finally {
      busy = false;
    }
  }

  async function scan() {
    scanning = true;
    error = null;
    try {
      const text = await api.android.scan();
      ticket = text.trim();
      await join(ticket);
    } catch (e) {
      const err = toUiError(e);
      // Closing the scanner is not an error.
      if (err.code !== "cancelled") error = err;
    } finally {
      scanning = false;
    }
  }
</script>

<div class="join">
  <header class="top">
    <a class="back" href={back}><Icon name="back" size={18} />Back</a>
    <h1>Join a mesh</h1>
    <p class="lead">
      Use an invite from a device already in your mesh. On that device, open
      <strong>Add device</strong>.
    </p>
  </header>

  <Card padding="lg" class="join-card">
    <form
      onsubmit={(e) => {
        e.preventDefault();
        void join(ticket);
      }}
    >
      <Field
        label="Device name"
        bind:value={name}
        autocomplete="off"
        maxlength={64}
        help="How your other devices will see this one."
      />

      {#if loaded && android}
        <div class="scan">
          <Button
            variant="primary"
            block
            busy={scanning || (busy && !pasting)}
            disabled={name.trim() === ""}
            onclick={scan}
          >
            <Icon name="scan" size={20} />Scan QR code
          </Button>
          {#if !pasting}
            <Button variant="ghost" block onclick={() => (pasting = true)}>Paste instead</Button>
          {/if}
        </div>
      {/if}

      {#if showPaste}
        <Field
          label="Invite"
          bind:value={ticket}
          mono
          placeholder="Paste the invite text"
          autocomplete="off"
          autocapitalize="off"
          spellcheck={false}
        />
      {/if}

      {#if error}
        <div class="error" role="alert">
          <Icon name="alert" size={18} />
          <p>
            <strong>{refusalText(error.reason) ?? error.message}</strong>
            {#if error.hint}<span class="hint">{error.hint}</span>{/if}
          </p>
        </div>
      {/if}

      {#if showPaste}
        <div class="actions">
          <Button
            variant="primary"
            type="submit"
            {busy}
            disabled={ticket.trim() === "" || name.trim() === ""}>Join mesh</Button
          >
        </div>
      {/if}
    </form>
  </Card>
</div>

<style>
  .join {
    display: grid;
    gap: var(--s-6);
    width: 100%;
    max-width: 560px;
    margin: 0 auto;
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
  .lead {
    color: var(--ink-2);
  }
  form {
    display: grid;
    gap: var(--s-5);
  }
  .scan {
    display: grid;
    gap: var(--s-2);
  }
  .scan :global(.btn.primary) {
    min-height: 56px;
    font-size: var(--fs-lg);
  }
  .error {
    display: flex;
    gap: var(--s-3);
    padding: var(--s-3) var(--s-4);
    border-radius: var(--r-sm);
    background: var(--danger-soft);
    color: var(--danger);
    font-size: var(--fs-sm);
  }
  .error :global(.icon) {
    margin-top: 1px;
  }
  .error strong {
    font-weight: var(--fw-semibold);
  }
  .hint {
    display: block;
    color: var(--ink-2);
  }
  .actions {
    display: flex;
    justify-content: flex-end;
  }
  @media (max-width: 639px) {
    .actions > :global(*) {
      flex: 1;
    }
  }
</style>
