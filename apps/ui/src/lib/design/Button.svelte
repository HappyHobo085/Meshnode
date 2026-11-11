<script lang="ts">
  import type { Snippet } from "svelte";
  import type { HTMLButtonAttributes } from "svelte/elements";

  interface Props extends HTMLButtonAttributes {
    /** primary: the one main action; secondary: default; ghost: low emphasis; danger: destructive. */
    variant?: "primary" | "secondary" | "ghost" | "danger";
    size?: "md" | "sm";
    /** Shows a spinner and disables the button while an action runs. */
    busy?: boolean;
    block?: boolean;
    children: Snippet;
  }
  let {
    variant = "secondary",
    size = "md",
    busy = false,
    block = false,
    disabled = false,
    type = "button",
    class: klass = "",
    children,
    ...rest
  }: Props = $props();
</script>

<button
  {...rest}
  {type}
  class={["btn", variant, size, block && "block", klass]}
  disabled={disabled || busy}
  aria-busy={busy || undefined}
>
  {#if busy}<span class="spinner" aria-hidden="true"></span>{/if}
  {@render children()}
</button>

<style>
  .btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: var(--s-2);
    min-height: var(--touch);
    padding: 0 var(--s-5);
    border: 1px solid transparent;
    border-radius: var(--r-sm);
    font-size: var(--fs-md);
    font-weight: var(--fw-medium);
    line-height: 1;
    white-space: nowrap;
    cursor: pointer;
    user-select: none;
    transition:
      background-color var(--dur-1) var(--ease-out),
      border-color var(--dur-1) var(--ease-out),
      transform var(--dur-1) var(--ease-out);
  }
  .btn:active:not(:disabled) {
    transform: translateY(1px);
  }
  .btn:disabled {
    cursor: not-allowed;
    opacity: 0.5;
  }
  .btn:focus-visible {
    border-radius: var(--r-sm);
  }
  .sm {
    min-height: 34px;
    padding: 0 var(--s-3);
    font-size: var(--fs-sm);
  }
  @media (pointer: coarse) {
    .sm {
      min-height: var(--touch);
    }
  }
  .block {
    display: flex;
    width: 100%;
  }

  .primary {
    background: var(--signal);
    color: var(--signal-ink);
  }
  .primary:hover:not(:disabled) {
    background: color-mix(in oklab, var(--signal) 88%, var(--ink));
  }

  .secondary {
    background: var(--surface-2);
    border-color: var(--line);
    color: var(--ink);
  }
  .secondary:hover:not(:disabled) {
    background: var(--surface-3);
  }

  .ghost {
    background: transparent;
    color: var(--ink-2);
  }
  .ghost:hover:not(:disabled) {
    background: var(--surface-2);
    color: var(--ink);
  }

  .danger {
    background: var(--danger);
    color: var(--danger-ink);
  }
  .danger:hover:not(:disabled) {
    background: color-mix(in oklab, var(--danger) 88%, var(--ink));
  }

  .spinner {
    width: 1em;
    height: 1em;
    border: 2px solid currentColor;
    border-right-color: transparent;
    border-radius: 50%;
    animation: spin 700ms linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>
