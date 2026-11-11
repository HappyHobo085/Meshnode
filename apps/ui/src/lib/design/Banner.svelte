<script lang="ts">
  import type { Snippet } from "svelte";
  import type { BannerState } from "../stores";
  import Icon from "./Icon.svelte";

  interface Props {
    state: BannerState | null;
    /** Platform actions: desktop (Run / Install / Locate), Android (Start). */
    actions?: Snippet<[BannerState]>;
  }
  let { state, actions }: Props = $props();

  const TEXT: Record<BannerState["kind"], string> = {
    unreachable: "Can't reach Meshnode on this device",
    stopped: "Node stopped",
    starting: "Meshnode is starting…",
  };
</script>

{#if state}
  <div
    class={["banner", state.kind]}
    role={state.kind === "starting" ? "status" : "alert"}
    aria-live={state.kind === "starting" ? "polite" : "assertive"}
  >
    <span class="icon">
      {#if state.kind === "starting"}
        <span class="spinner" aria-hidden="true"></span>
      {:else}
        <Icon name={state.kind === "stopped" ? "power" : "alert"} size={18} />
      {/if}
    </span>
    <div class="text">
      <strong>{TEXT[state.kind]}</strong>
      {#if state.error?.hint}<span class="hint">{state.error.hint}</span>{/if}
    </div>
    {#if actions}<div class="actions">{@render actions(state)}</div>{/if}
  </div>
{/if}

<style>
  .banner {
    display: flex;
    align-items: center;
    gap: var(--s-3);
    padding: var(--s-3) var(--s-4);
    border-bottom: 1px solid var(--line);
    background: var(--warn-soft);
    color: var(--ink);
  }
  .banner.unreachable {
    background: var(--danger-soft);
  }
  .banner.starting {
    background: var(--signal-soft);
  }
  .icon {
    display: grid;
    place-items: center;
    color: var(--warn);
  }
  .unreachable .icon {
    color: var(--danger);
  }
  .starting .icon {
    color: var(--signal);
  }
  .text {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    column-gap: var(--s-3);
    flex: 1;
    min-width: 0;
    font-size: var(--fs-sm);
  }
  strong {
    font-weight: var(--fw-semibold);
  }
  .hint {
    color: var(--ink-2);
  }
  .actions {
    display: flex;
    gap: var(--s-2);
    flex-wrap: wrap;
  }
  .spinner {
    width: 16px;
    height: 16px;
    border: 2px solid currentColor;
    border-right-color: transparent;
    border-radius: 50%;
    animation: spin 800ms linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  @media (max-width: 639px) {
    .banner {
      flex-wrap: wrap;
    }
    .actions {
      width: 100%;
    }
  }
</style>
