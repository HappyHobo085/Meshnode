<script lang="ts">
  import { onMount } from "svelte";
  import { api, toUiError, type UiError } from "../lib/api";
  import Button from "../lib/design/Button.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import { navigate } from "../lib/router";
  import { banner, refreshNow, status } from "../lib/stores";

  let starting = $state(false);
  let error: UiError | null = $state(null);

  // The node answers again (started here or by the system): back to Home.
  $effect(() => {
    if ($status !== null && $banner === null) navigate({ name: "home" }, { replace: true });
  });

  // Started meanwhile (from the notification, say): refresh so the effect above moves on.
  onMount(() => {
    api.android
      .nodeRunning()
      .then((running) => {
        if (running) void refreshNow();
      })
      .catch(() => {});
  });

  async function start() {
    if (starting) return;
    starting = true;
    error = null;
    try {
      await api.android.serviceStart();
      await refreshNow();
    } catch (e) {
      error = toUiError(e);
    } finally {
      starting = false;
    }
  }
</script>

<section class="stopped">
  <span class="mark" aria-hidden="true"><Icon name="power" size={30} /></span>
  <h1>Node stopped</h1>
  <p class="lede">
    This phone isn't taking part in your mesh right now. Its services don't answer and it doesn't
    sync. Start it to rejoin your other devices.
  </p>
  <Button variant="primary" block onclick={start} busy={starting}>Start</Button>
  {#if starting}
    <p class="progress" role="status">Starting…</p>
  {/if}
  {#if error}
    <p class="error" role="alert">
      {error.message}{#if error.hint}<span class="hint">{error.hint}</span>{/if}
    </p>
  {/if}
</section>

<style>
  .stopped {
    display: grid;
    justify-items: center;
    gap: var(--s-4);
    max-width: 420px;
    margin: 0 auto;
    padding: clamp(var(--s-10), 16vh, 160px) var(--s-2) var(--s-8);
    text-align: center;
  }
  .mark {
    display: grid;
    place-items: center;
    width: 72px;
    height: 72px;
    margin-bottom: var(--s-2);
    border-radius: 50%;
    background: var(--warn-soft);
    color: var(--warn);
  }
  .lede {
    color: var(--ink-2);
    margin-bottom: var(--s-4);
  }
  .progress {
    color: var(--ink-3);
    font-size: var(--fs-sm);
  }
  .error {
    color: var(--danger);
    font-size: var(--fs-sm);
  }
  .hint {
    display: block;
    color: var(--ink-2);
  }
</style>
