<script lang="ts">
  import Button from "../lib/design/Button.svelte";
  import Field from "../lib/design/Field.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import { MAX_OUTBOUND, validOutbound } from "../lib/service";

  interface Props {
    /** The allowlist shown; changes come back through `onchange`. */
    value: string[];
    onchange: (list: string[]) => void;
  }
  let { value, onchange }: Props = $props();

  let entry = $state("");
  let error = $state<string | null>(null);

  function add(e?: Event) {
    e?.preventDefault();
    const v = entry.trim();
    if (!validOutbound(v)) {
      error = "Use host:port";
      return;
    }
    if (value.length >= MAX_OUTBOUND && !value.includes(v)) {
      error = `Up to ${MAX_OUTBOUND} addresses`;
      return;
    }
    error = null;
    entry = "";
    if (!value.includes(v)) onchange([...value, v]);
  }
</script>

<div class="outbound">
  {#if value.length > 0}
    <ul class="entries" aria-label="Allowed addresses">
      {#each value as v (v)}
        <li>
          <span class="addr mono truncate" title={v}>{v}</span>
          <button
            type="button"
            class="remove"
            aria-label="Remove {v}"
            onclick={() => onchange(value.filter((x) => x !== v))}
          >
            <Icon name="close" size={14} />
          </button>
        </li>
      {/each}
    </ul>
  {:else}
    <p class="none">It can't open connections outside the mesh.</p>
  {/if}

  <form class="add" onsubmit={add} novalidate>
    <Field
      label="Add an address"
      bind:value={entry}
      mono
      placeholder="api.example.com:443"
      autocomplete="off"
      autocapitalize="off"
      spellcheck={false}
      oninput={() => (error = null)}
      {error}
    />
    <Button type="submit" aria-label="Add address" disabled={!entry.trim()}>Add</Button>
  </form>
</div>

<style>
  .outbound {
    display: grid;
    gap: var(--s-3);
    min-width: 0;
  }
  .entries {
    display: grid;
    margin: 0;
    padding: 0;
    list-style: none;
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    background: var(--surface-2);
  }
  .entries li {
    display: flex;
    align-items: center;
    gap: var(--s-2);
    min-width: 0;
    padding: 2px 4px 2px var(--s-3);
  }
  .entries li + li {
    border-top: 1px solid var(--line);
  }
  .addr {
    flex: 1;
    color: var(--ink-2);
    font-size: var(--fs-sm);
  }
  .remove {
    display: grid;
    place-items: center;
    flex: none;
    width: 32px;
    height: 32px;
    border: 0;
    border-radius: var(--r-xs);
    background: transparent;
    color: var(--ink-3);
    cursor: pointer;
  }
  .remove:hover {
    background: var(--surface-3);
    color: var(--danger);
  }
  @media (pointer: coarse) {
    .remove {
      width: var(--touch);
      height: var(--touch);
    }
  }
  .none {
    font-size: var(--fs-sm);
    color: var(--ink-3);
  }
  .add {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    align-items: start;
    gap: var(--s-2);
    max-width: 440px;
  }
  /* The button lines up with the input, below the field's label. */
  .add > :global(.btn) {
    margin-top: calc(var(--fs-sm) * 1.5 + var(--s-2));
  }
</style>
