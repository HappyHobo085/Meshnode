<script lang="ts">
  import type { MemberDto, PlacementDto } from "../lib/api";
  import Button from "../lib/design/Button.svelte";
  import Chip from "../lib/design/Chip.svelte";
  import Monogram from "../lib/design/Monogram.svelte";
  import { listNames, MAX_TAG, selectedBy } from "../lib/service";

  interface Props {
    /** The placement shown; the editor is controlled, changes come back through `onchange`. */
    value: PlacementDto;
    onchange: (p: PlacementDto) => void;
    /** Effective hosts: the devices a placement can select. */
    hosts: MemberDto[];
    /** The service's name, for the preview sentence. */
    service?: string;
  }
  let { value, onchange, hosts, service = "" }: Props = $props();

  const uid = $props.id();
  const MODES: { kind: PlacementDto["kind"]; label: string }[] = [
    { kind: "allHosts", label: "All hosts" },
    { kind: "tags", label: "Hosts with tags" },
    { kind: "nodes", label: "Chosen hosts" },
  ];

  // What each mode had selected, so switching modes and back doesn't lose a choice.
  let lastTags = $state<string[]>([]);
  let lastIds = $state<string[]>([]);
  const tags = $derived(value.kind === "tags" ? value.tags : lastTags);
  const ids = $derived(value.kind === "nodes" ? value.ids : lastIds);

  let extraTags = $state<string[]>([]);
  let newTag = $state("");
  let tagError = $state<string | null>(null);

  /** Every tag a host carries, plus any already chosen or typed here. */
  const allTags = $derived(
    [...new Set([...hosts.flatMap((h) => h.tags), ...tags, ...extraTags])].sort((a, b) =>
      a.localeCompare(b),
    ),
  );
  const chosen = $derived(selectedBy(value, hosts));
  const unknownIds = $derived(ids.filter((id) => !hosts.some((h) => h.id === id)));

  const preview = $derived.by(() => {
    if (value.kind === "tags" && value.tags.length === 0) return "Choose at least one tag.";
    if (value.kind === "nodes" && value.ids.length === 0) return "Choose at least one host.";
    if (chosen.length === 0) {
      return value.kind === "allHosts"
        ? "There are no hosts yet. It will run on each host that joins."
        : "No host matches yet. It runs nowhere until one does.";
    }
    const who = listNames(chosen.map((h) => h.name));
    return `${service || "It"} will run on ${who}.`;
  });

  function setMode(kind: PlacementDto["kind"]) {
    if (value.kind === "tags") lastTags = value.tags;
    if (value.kind === "nodes") lastIds = value.ids;
    if (kind === "allHosts") onchange({ kind });
    else if (kind === "tags") onchange({ kind, tags: [...tags] });
    else onchange({ kind, ids: [...ids] });
  }

  function toggleTag(t: string) {
    const next = tags.includes(t) ? tags.filter((x) => x !== t) : [...tags, t];
    onchange({ kind: "tags", tags: next });
  }

  function toggleHost(id: string) {
    const next = ids.includes(id) ? ids.filter((x) => x !== id) : [...ids, id];
    onchange({ kind: "nodes", ids: next });
  }

  function addTag(e?: Event) {
    e?.preventDefault();
    const t = newTag.trim();
    if (!t) return;
    if (t.length > MAX_TAG || /\s/.test(t)) {
      tagError = `Use one word of up to ${MAX_TAG} characters`;
      return;
    }
    tagError = null;
    newTag = "";
    if (!allTags.includes(t)) extraTags = [...extraTags, t];
    if (!tags.includes(t)) onchange({ kind: "tags", tags: [...tags, t] });
  }
</script>

<div class="placement">
  <div class="segments" role="radiogroup" aria-label="Where it runs">
    {#each MODES as m (m.kind)}
      <label class={["segment", { on: value.kind === m.kind }]}>
        <input
          type="radio"
          name="placement-{uid}"
          value={m.kind}
          checked={value.kind === m.kind}
          onchange={() => setMode(m.kind)}
        />
        <span>{m.label}</span>
      </label>
    {/each}
  </div>

  {#if value.kind === "tags"}
    <div class="choices wrap" role="group" aria-label="Tags">
      {#each allTags as t (t)}
        <Chip label={t} selected={tags.includes(t)} onclick={() => toggleTag(t)} />
      {/each}
      {#if allTags.length === 0}
        <p class="none">No host has a tag yet. Add one here, then give it to a host.</p>
      {/if}
    </div>
    <div class="add-tag">
      <label class="visually-hidden" for="tag-{uid}">Add a tag</label>
      <input
        id="tag-{uid}"
        class={["tag-input", { invalid: !!tagError }]}
        placeholder="Add a tag"
        autocomplete="off"
        autocapitalize="off"
        spellcheck={false}
        bind:value={newTag}
        aria-invalid={tagError ? "true" : undefined}
        aria-describedby={tagError ? `tag-${uid}-error` : undefined}
        onkeydown={(e) => e.key === "Enter" && addTag(e)}
      />
      <Button size="sm" onclick={addTag} disabled={!newTag.trim()}>Add</Button>
    </div>
    {#if tagError}<p class="error" id="tag-{uid}-error" role="alert">{tagError}</p>{/if}
  {:else if value.kind === "nodes"}
    <div class="choices wrap" role="group" aria-label="Hosts">
      {#each hosts as h (h.id)}
        {@const on = ids.includes(h.id)}
        <button
          type="button"
          class={["host", { on }]}
          style:--h={h.accentHue}
          aria-pressed={on}
          onclick={() => toggleHost(h.id)}
        >
          <Monogram name={h.name} hue={h.accentHue} size="sm" />
          <span class="truncate">{h.name}</span>
        </button>
      {/each}
      {#each unknownIds as id (id)}
        <Chip label={id.slice(0, 8)} mono selected onclick={() => toggleHost(id)} />
      {/each}
      {#if hosts.length === 0 && unknownIds.length === 0}
        <p class="none">There are no hosts yet.</p>
      {/if}
    </div>
  {/if}

  <p class={["preview", { warn: chosen.length === 0 && value.kind !== "allHosts" }]}>
    {preview}
  </p>
</div>

<style>
  .placement {
    display: grid;
    gap: var(--s-3);
    min-width: 0;
  }

  /* The three modes: a segmented control built from radios. */
  .segments {
    display: inline-flex;
    flex-wrap: wrap;
    justify-self: start;
    max-width: 100%;
    padding: 3px;
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    background: var(--surface-2);
  }
  .segment {
    position: relative;
    display: grid;
    place-items: center;
    min-height: 34px;
    padding: 0 var(--s-3);
    border-radius: 6px;
    color: var(--ink-2);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    white-space: nowrap;
    cursor: pointer;
  }
  .segment input {
    position: absolute;
    opacity: 0;
    pointer-events: none;
  }
  .segment.on {
    background: var(--surface);
    color: var(--ink);
    box-shadow: var(--e-1);
  }
  .segment:has(input:focus-visible) {
    box-shadow: var(--focus-ring);
  }
  @media (pointer: coarse) {
    .segment {
      min-height: var(--touch);
    }
    .host {
      min-height: var(--touch);
    }
    .tag-input {
      min-height: var(--touch);
    }
  }

  .choices {
    gap: var(--s-2);
    align-items: center;
  }

  /* A host to pick: its monogram and name, edged in its hue when chosen. */
  .host {
    display: inline-flex;
    align-items: center;
    gap: var(--s-2);
    max-width: 100%;
    min-height: 38px;
    padding: 4px var(--s-3) 4px 4px;
    border: 1px solid var(--line);
    border-radius: var(--r-pill);
    background: var(--surface);
    color: var(--ink-2);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    cursor: pointer;
    transition:
      border-color var(--dur-1) var(--ease-out),
      background-color var(--dur-1) var(--ease-out);
  }
  .host :global(.mono-tile) {
    border-radius: 50%;
  }
  .host:hover {
    border-color: var(--line-strong);
    color: var(--ink);
  }
  .host.on {
    border-color: var(--accent);
    background: var(--accent-soft);
    color: var(--accent-ink);
  }
  .host:focus-visible {
    border-radius: var(--r-pill);
  }

  .add-tag {
    display: flex;
    gap: var(--s-2);
    max-width: 320px;
  }
  .tag-input {
    flex: 1;
    min-width: 0;
    min-height: 34px;
    padding: 0 var(--s-3);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    background: var(--surface);
    font-size: var(--fs-sm);
  }
  .tag-input::placeholder {
    color: var(--ink-3);
  }
  .tag-input:focus {
    border-color: var(--signal);
    box-shadow: 0 0 0 3px var(--signal-soft);
    border-radius: var(--r-sm);
  }
  .tag-input.invalid {
    border-color: var(--danger);
  }

  .none,
  .preview {
    font-size: var(--fs-sm);
    color: var(--ink-2);
  }
  .preview.warn {
    color: var(--warn);
  }
  .error {
    font-size: var(--fs-sm);
    color: var(--danger);
  }
</style>
