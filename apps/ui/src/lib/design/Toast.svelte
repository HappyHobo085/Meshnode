<script lang="ts">
  import { fly } from "svelte/transition";
  import { dismissToast, toasts } from "../stores";
  import Icon from "./Icon.svelte";
  import { dur, easeOut } from "./motion";
</script>

<!-- Toasts are for success only; errors stay next to the control that failed. -->
<div class="toasts" role="status" aria-live="polite">
  {#each $toasts as t (t.id)}
    <div class="toast" transition:fly={{ y: 12, duration: dur(160), easing: easeOut }}>
      <span class="tick"><Icon name="check" size={16} /></span>
      <span class="msg">{t.message}</span>
      <button type="button" class="x" aria-label="Dismiss" onclick={() => dismissToast(t.id)}>
        <Icon name="close" size={14} />
      </button>
    </div>
  {/each}
</div>

<style>
  .toasts {
    position: fixed;
    z-index: 60;
    left: 50%;
    bottom: calc(var(--s-6) + var(--safe-bottom));
    transform: translateX(-50%);
    display: grid;
    gap: var(--s-2);
    width: max-content;
    max-width: calc(100vw - var(--s-8));
    pointer-events: none;
  }
  .toast {
    display: flex;
    align-items: center;
    gap: var(--s-3);
    padding: var(--s-2) var(--s-2) var(--s-2) var(--s-3);
    background: var(--ink);
    color: var(--bg);
    border-radius: var(--r-md);
    box-shadow: var(--e-3);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    pointer-events: auto;
  }
  .tick {
    display: grid;
    place-items: center;
    width: 24px;
    height: 24px;
    border-radius: 50%;
    background: var(--ok);
    color: var(--surface);
  }
  .x {
    display: grid;
    place-items: center;
    width: 28px;
    height: 28px;
    border: 0;
    border-radius: var(--r-sm);
    background: transparent;
    color: inherit;
    opacity: 0.7;
    cursor: pointer;
  }
  .x:hover {
    opacity: 1;
  }
  @media (max-width: 639px) {
    .toasts {
      bottom: calc(var(--tabbar-h) + var(--s-4) + var(--safe-bottom));
    }
  }
</style>
