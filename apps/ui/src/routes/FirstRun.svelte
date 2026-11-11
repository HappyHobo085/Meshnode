<script lang="ts">
  import { onMount } from "svelte";
  import { api, toUiError, type UiError } from "../lib/api";
  import Button from "../lib/design/Button.svelte";
  import Card from "../lib/design/Card.svelte";
  import Field from "../lib/design/Field.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import Monogram from "../lib/design/Monogram.svelte";
  import { chosenName, defaultDeviceName } from "../lib/device";
  import { navigate } from "../lib/router";
  import { refreshNow, status, toast } from "../lib/stores";

  /** The hue the preview tile uses before this device has an id (the app's own teal). */
  const PREVIEW_HUE = 172;

  let name = $state($chosenName ?? $status?.name ?? "");
  let busy = $state(false);
  let error = $state<UiError | null>(null);
  const trimmed = $derived(name.trim());

  onMount(() => {
    api
      .platform()
      .then((p) => {
        // Only fill the field if the user hasn't started typing.
        if (name === "") name = defaultDeviceName(p);
      })
      .catch(() => {});
  });

  async function create() {
    if (!trimmed) return;
    busy = true;
    error = null;
    try {
      await api.init(trimmed);
      chosenName.set(null);
      toast("Your mesh is ready");
      // Wait for the new state, or App would still see "uninitialised" and send us back.
      await refreshNow();
      navigate({ name: "home" }, { replace: true });
    } catch (e) {
      error = toUiError(e);
    } finally {
      busy = false;
    }
  }

  function toJoin() {
    chosenName.set(trimmed || null);
    navigate({ name: "join" });
  }
</script>

<div class="first-run">
  <header class="intro">
    <span class="brand"><Icon name="mesh" size={20} />Meshnode</span>
    <h1>Set up this device</h1>
    <p class="lead">
      Meshnode links the devices you own into one private network. Start a new one here, or join one
      you already have.
    </p>
  </header>

  <!-- The device's name, with a live preview of the tile your other devices will show. -->
  <div class="naming">
    <Monogram name={trimmed || "?"} hue={PREVIEW_HUE} size="xl" />
    <Field
      label="Device name"
      bind:value={name}
      autocomplete="off"
      maxlength={64}
      help="How your other devices will see this one. You can change it later."
    />
  </div>

  <div class="choices">
    <Card padding="lg" class="choice">
      <span class="choice-icon"><Icon name="mesh" size={22} /></span>
      <h2>Start a new mesh</h2>
      <p>This device becomes the first member. You'll add your other devices from Home.</p>
      {#if error}
        <p class="error" role="alert">
          {error.message}{#if error.hint}<span class="hint">{error.hint}</span>{/if}
        </p>
      {/if}
      <Button variant="primary" block {busy} disabled={!trimmed} onclick={create}>
        Create mesh
      </Button>
    </Card>

    <Card padding="lg" class="choice">
      <span class="choice-icon"><Icon name="qr" size={22} /></span>
      <h2>Join a mesh you have</h2>
      <p>Already use Meshnode on another device? Scan or paste an invite from it.</p>
      <Button block onclick={toJoin}>Join a mesh</Button>
    </Card>
  </div>
</div>

<style>
  .first-run {
    display: grid;
    gap: var(--s-8);
    width: 100%;
    max-width: 760px;
    margin: 0 auto;
    padding-top: var(--s-10);
  }
  .intro {
    display: grid;
    gap: var(--s-3);
  }
  .brand {
    display: inline-flex;
    align-items: center;
    gap: var(--s-2);
    color: var(--signal);
    font-weight: var(--fw-semibold);
  }
  h1 {
    font-size: var(--fs-3xl);
    font-weight: var(--fw-bold);
    letter-spacing: -0.03em;
  }
  .lead {
    max-width: 52ch;
    color: var(--ink-2);
    font-size: var(--fs-lg);
    line-height: var(--lh-snug);
  }

  .naming {
    display: flex;
    align-items: flex-start;
    gap: var(--s-5);
    max-width: 520px;
  }
  .naming :global(.field) {
    flex: 1;
    min-width: 0;
  }

  .choices {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(100%, 300px), 1fr));
    gap: var(--s-4);
  }
  :global(.card.choice) {
    display: flex;
    flex-direction: column;
    gap: var(--s-3);
  }
  :global(.card.choice) p {
    flex: 1;
    color: var(--ink-2);
  }
  :global(.card.choice) .error {
    flex: none;
    font-size: var(--fs-sm);
    color: var(--danger);
  }
  .hint {
    display: block;
    color: var(--ink-2);
  }
  .choice-icon {
    display: grid;
    place-items: center;
    width: 44px;
    height: 44px;
    border-radius: var(--r-md);
    background: var(--signal-soft);
    color: var(--signal);
  }
  h2 {
    font-size: var(--fs-lg);
  }

  @media (max-width: 639px) {
    .first-run {
      gap: var(--s-6);
      padding-top: var(--s-4);
    }
    h1 {
      font-size: var(--fs-2xl);
    }
    .lead {
      font-size: var(--fs-md);
    }
    .naming {
      gap: var(--s-4);
    }
    .naming :global(.mono-tile) {
      --size: 56px;
    }
  }
</style>
