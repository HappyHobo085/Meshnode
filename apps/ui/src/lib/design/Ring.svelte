<script lang="ts">
  import type { Snippet } from "svelte";

  interface Props {
    /** How much is left, 0–1 (the invite countdown). */
    fraction: number;
    size?: number;
    stroke?: number;
    /** Accessible description, e.g. "Expires in 12 minutes". */
    label: string;
    children?: Snippet;
  }
  let { fraction, size = 96, stroke = 4, label, children }: Props = $props();

  const r = $derived((size - stroke) / 2);
  const circumference = $derived(2 * Math.PI * r);
  const clamped = $derived(Math.min(1, Math.max(0, fraction)));
  // SVG presentation attributes (not inline style), so the strict CSP is satisfied.
  const offset = $derived(circumference * (1 - clamped));
</script>

<div class="ring" role="timer" aria-label={label}>
  <svg width={size} height={size} viewBox="0 0 {size} {size}" aria-hidden="true">
    <circle class="track" cx={size / 2} cy={size / 2} {r} stroke-width={stroke} fill="none" />
    <circle
      class={["arc", { low: clamped < 0.2 }]}
      cx={size / 2}
      cy={size / 2}
      {r}
      stroke-width={stroke}
      fill="none"
      stroke-linecap="round"
      stroke-dasharray={circumference}
      stroke-dashoffset={offset}
      transform="rotate(-90 {size / 2} {size / 2})"
    />
  </svg>
  {#if children}<div class="center">{@render children()}</div>{/if}
</div>

<style>
  .ring {
    position: relative;
    display: inline-grid;
    place-items: center;
  }
  .ring > svg {
    grid-area: 1 / 1;
  }
  .center {
    grid-area: 1 / 1;
    text-align: center;
    font-variant-numeric: tabular-nums;
  }
  .track {
    stroke: var(--line);
  }
  .arc {
    stroke: var(--signal);
    transition:
      stroke-dashoffset 1s linear,
      stroke var(--dur-2) var(--ease-out);
  }
  .arc.low {
    stroke: var(--warn);
  }
  @media (prefers-reduced-motion: reduce) {
    .arc {
      transition: none;
    }
  }
</style>
