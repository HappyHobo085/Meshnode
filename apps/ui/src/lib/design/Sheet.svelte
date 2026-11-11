<script lang="ts">
  import type { Snippet } from "svelte";
  import { fade, fly } from "svelte/transition";
  import Icon from "./Icon.svelte";
  import { dur, easeOut } from "./motion";

  interface Props {
    open: boolean;
    title: string;
    onclose: () => void;
    /** Wide sheets (service detail, deploy) get more room on desktop. */
    wide?: boolean;
    children: Snippet;
    /** Actions pinned to the sheet's bottom edge. */
    footer?: Snippet;
  }
  let { open, title, onclose, wide = false, children, footer }: Props = $props();

  const uid = $props.id();
  const titleId = `sheet-${uid}`;
  let panel: HTMLElement | undefined = $state();
  let returnFocus: HTMLElement | null = null;

  const FOCUSABLE =
    'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

  $effect(() => {
    if (!open || !panel) return;
    returnFocus = document.activeElement as HTMLElement | null;
    const first = panel.querySelector<HTMLElement>("input, textarea, select") ?? panel;
    first.focus();
    return () => returnFocus?.focus?.();
  });

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.stopPropagation();
      onclose();
      return;
    }
    if (e.key !== "Tab" || !panel) return;
    // Keep focus inside the sheet.
    const items = [...panel.querySelectorAll<HTMLElement>(FOCUSABLE)];
    const firstItem = items[0];
    const lastItem = items[items.length - 1];
    if (!firstItem || !lastItem) return;
    if (e.shiftKey && document.activeElement === firstItem) {
      e.preventDefault();
      lastItem.focus();
    } else if (!e.shiftKey && document.activeElement === lastItem) {
      e.preventDefault();
      firstItem.focus();
    }
  }
</script>

{#if open}
  <div class="layer">
    <div
      class="scrim"
      transition:fade={{ duration: dur(160) }}
      onclick={onclose}
      aria-hidden="true"
    ></div>
    <div
      bind:this={panel}
      class={["panel", { wide }]}
      role="dialog"
      aria-modal="true"
      aria-labelledby={titleId}
      tabindex="-1"
      {onkeydown}
      transition:fly={{ y: 24, duration: dur(200), easing: easeOut }}
    >
      <header>
        <h2 id={titleId}>{title}</h2>
        <button type="button" class="close" aria-label="Close" onclick={onclose}>
          <Icon name="close" size={18} />
        </button>
      </header>
      <div class="body">{@render children()}</div>
      {#if footer}<footer>{@render footer()}</footer>{/if}
    </div>
  </div>
{/if}

<style>
  .layer {
    position: fixed;
    inset: 0;
    z-index: 50;
    display: grid;
    place-items: center;
    padding: var(--s-6);
  }
  .scrim {
    position: absolute;
    inset: 0;
    background: var(--scrim);
  }
  .panel {
    position: relative;
    display: flex;
    flex-direction: column;
    width: min(480px, 100%);
    max-height: min(720px, 100%);
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--r-lg);
    box-shadow: var(--e-3);
    overflow: hidden;
  }
  .panel.wide {
    width: min(680px, 100%);
  }
  .panel:focus-visible {
    border-radius: var(--r-lg);
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--s-4);
    padding: var(--s-5) var(--s-6) var(--s-3);
  }
  h2 {
    min-width: 0;
    font-size: var(--fs-lg);
    overflow-wrap: anywhere;
  }
  .close {
    /* A long title wraps instead of squeezing the button below its target size. */
    flex-shrink: 0;
    display: grid;
    place-items: center;
    width: 36px;
    height: 36px;
    margin-right: calc(var(--s-2) * -1);
    border: 0;
    border-radius: var(--r-sm);
    background: transparent;
    color: var(--ink-2);
    cursor: pointer;
  }
  @media (pointer: coarse) {
    .close {
      width: var(--touch);
      height: var(--touch);
    }
  }
  .close:hover {
    background: var(--surface-2);
    color: var(--ink);
  }
  .body {
    padding: var(--s-2) var(--s-6) var(--s-6);
    overflow-y: auto;
    overscroll-behavior: contain;
  }
  footer {
    display: flex;
    justify-content: flex-end;
    gap: var(--s-3);
    padding: var(--s-4) var(--s-6);
    border-top: 1px solid var(--line);
    background: var(--surface);
  }

  /* Phones: a bottom sheet, edge to edge, above the home indicator. */
  @media (max-width: 639px) {
    .layer {
      place-items: end stretch;
      padding: var(--s-4) 0 0;
    }
    .panel,
    .panel.wide {
      width: 100%;
      max-height: calc(100dvh - var(--safe-top) - var(--s-8));
      border-radius: var(--r-lg) var(--r-lg) 0 0;
      border-bottom: 0;
    }
    .panel::before {
      content: "";
      align-self: center;
      width: 36px;
      height: 4px;
      margin-top: var(--s-2);
      border-radius: var(--r-pill);
      background: var(--line-strong);
    }
    .body {
      padding-left: var(--s-5);
      padding-right: var(--s-5);
    }
    footer {
      padding: var(--s-3) var(--s-5) calc(var(--s-3) + var(--safe-bottom));
    }
    footer > :global(*) {
      flex: 1;
    }
  }
</style>
