<script lang="ts">
  import type { Snippet } from "svelte";
  import type { HTMLAttributes } from "svelte/elements";

  interface Props extends HTMLAttributes<HTMLElement> {
    /** A device's accent hue: draws its edge down the card's left side. */
    hue?: number | null;
    /** Animates in once, when a member appears. */
    arrive?: boolean;
    /** Dims the card, e.g. an offline device. */
    muted?: boolean;
    padding?: "md" | "lg";
    children: Snippet;
  }
  let {
    hue = null,
    arrive = false,
    muted = false,
    padding = "md",
    class: klass = "",
    children,
    ...rest
  }: Props = $props();
</script>

<article
  {...rest}
  class={["card", padding, klass, { edged: hue !== null, arrive, muted }]}
  style:--h={hue ?? undefined}
>
  {@render children()}
</article>

<style>
  .card {
    position: relative;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--r-md);
    box-shadow: var(--e-1);
    overflow: hidden;
  }
  .md {
    padding: var(--s-4);
  }
  .lg {
    padding: var(--s-6);
  }
  /* The device edge: a 3 px seam of the device's own hue, the thread that ties a device's
     card, monogram and chips together on every screen. */
  .edged::before {
    content: "";
    position: absolute;
    inset: 0 auto 0 0;
    width: 3px;
    background: var(--accent);
  }
  .edged.md {
    padding-left: calc(var(--s-4) + 3px);
  }
  .edged.lg {
    padding-left: calc(var(--s-6) + 3px);
  }
  .muted::before {
    background: color-mix(in oklab, var(--accent) 35%, var(--line));
  }
  .arrive {
    animation: mesh-arrive var(--dur-3) var(--ease-out) both;
  }
</style>
