<script lang="ts">
  import Icon from "./Icon.svelte";

  interface Props {
    label: string;
    /** A toggle chip (filters, placement tags): renders a pressed/unpressed button. */
    selected?: boolean;
    onclick?: () => void;
    /** Adds a remove button (tag editors). */
    onremove?: () => void;
    /** Tints the chip with a device's hue. */
    hue?: number | null;
    mono?: boolean;
  }
  let { label, selected, onclick, onremove, hue = null, mono = false }: Props = $props();
</script>

{#if onclick}
  <button
    type="button"
    class={["chip", "toggle", { selected, hued: hue !== null, mono }]}
    style:--h={hue ?? undefined}
    aria-pressed={selected ?? false}
    {onclick}
  >
    {#if selected}<Icon name="check" size={14} />{/if}
    {label}
  </button>
{:else}
  <span class={["chip", { hued: hue !== null, mono }]} style:--h={hue ?? undefined}>
    {label}
    {#if onremove}
      <button type="button" class="remove" aria-label="Remove {label}" onclick={onremove}>
        <Icon name="close" size={12} />
      </button>
    {/if}
  </span>
{/if}

<style>
  .chip {
    display: inline-flex;
    align-items: center;
    gap: var(--s-1);
    height: 28px;
    padding: 0 var(--s-3);
    border: 1px solid var(--line);
    border-radius: var(--r-pill);
    background: var(--surface-2);
    color: var(--ink-2);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    white-space: nowrap;
  }
  .mono {
    font-family: var(--font-mono);
    font-size: var(--fs-xs);
  }
  .hued {
    background: var(--accent-soft);
    border-color: transparent;
    color: var(--accent-ink);
  }
  .toggle {
    cursor: pointer;
    transition:
      background-color var(--dur-1) var(--ease-out),
      color var(--dur-1) var(--ease-out);
  }
  .toggle:hover {
    color: var(--ink);
    border-color: var(--line-strong);
  }
  .toggle:focus-visible {
    border-radius: var(--r-pill);
  }
  .toggle.selected {
    background: var(--signal-soft);
    border-color: color-mix(in oklab, var(--signal) 50%, transparent);
    color: var(--signal);
  }
  /* Toggle chips are controls: on touch screens they reach the 44 px target. */
  @media (pointer: coarse) {
    .toggle {
      height: var(--touch);
    }
  }
  .remove {
    display: inline-grid;
    place-items: center;
    width: 18px;
    height: 18px;
    margin-right: calc(var(--s-1) * -1);
    padding: 0;
    border: 0;
    border-radius: 50%;
    background: transparent;
    color: inherit;
    cursor: pointer;
  }
  .remove:hover {
    background: var(--surface-3);
  }
</style>
