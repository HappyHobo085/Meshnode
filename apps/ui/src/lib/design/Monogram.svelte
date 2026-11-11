<script lang="ts">
  import { monogram } from "../monogram";

  interface Props {
    name: string;
    /** The device's accent hue (`accentHue` from Rust). */
    hue: number;
    size?: "sm" | "md" | "lg" | "xl";
    /** Shows the online state as a dot on the tile's corner (with text for screen readers). */
    online?: boolean | null;
  }
  let { name, hue, size = "md", online = null }: Props = $props();
  const letters = $derived(monogram(name));
</script>

<span class={["mono-tile", size]} style:--h={hue} aria-hidden="true">
  <span class="letters">{letters}</span>
  {#if online !== null}
    <span class={["dot", { online }]}></span>
  {/if}
</span>

<style>
  /* A squircle-ish tile in the device's hue: soft fill, deep ink, a hairline of the hue. */
  .mono-tile {
    position: relative;
    display: inline-grid;
    place-items: center;
    flex: none;
    width: var(--size);
    height: var(--size);
    border-radius: calc(var(--size) * 0.32);
    background: var(--accent-soft);
    box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--accent) 45%, transparent);
    color: var(--accent-ink);
    font-weight: var(--fw-bold);
    font-size: calc(var(--size) * 0.38);
    letter-spacing: 0.02em;
    line-height: 1;
  }
  .sm {
    --size: 28px;
  }
  .md {
    --size: 40px;
  }
  .lg {
    --size: 56px;
  }
  .xl {
    --size: 72px;
  }
  .letters {
    font-feature-settings: "case" 1;
  }
  .dot {
    position: absolute;
    right: -2px;
    bottom: -2px;
    width: calc(var(--size) * 0.28);
    height: calc(var(--size) * 0.28);
    min-width: 9px;
    min-height: 9px;
    border-radius: 50%;
    background: var(--ink-3);
    border: 2px solid var(--surface);
  }
  .dot.online {
    background: var(--ok);
    animation: mesh-pulse 2.4s var(--ease-in-out) infinite;
  }
</style>
