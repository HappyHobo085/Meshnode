<script lang="ts">
  import { onMount } from "svelte";
  import { api, toUiError, type PowerPolicy, type UiError } from "../lib/api";

  // The three policies (ruling R6) and their battery trade-off, from mesh-host `power.rs`.
  const POLICIES: { value: PowerPolicy; title: string; text: string; cost: string }[] = [
    {
      value: "balanced",
      title: "Balanced",
      text: "Stays awake while charging. On battery it wakes to answer requests and to catch up, then sleeps.",
      cost: "Moderate battery use",
    },
    {
      value: "always-on",
      title: "Always on",
      text: "Keeps the phone awake all the time, so its services answer at once.",
      cost: "Most battery use",
    },
    {
      value: "saver",
      title: "Saver",
      text: "Checks in every 15 minutes. With the screen off it stops serving other devices.",
      cost: "Least battery use",
    },
  ];

  const uid = $props.id();
  let policy: PowerPolicy | null = $state(null);
  let saving: PowerPolicy | null = $state(null);
  let error: UiError | null = $state(null);

  onMount(() => {
    api.android
      .powerPolicyGet()
      .then((p) => (policy = p))
      .catch((e: unknown) => (error = toUiError(e)));
  });

  async function choose(next: PowerPolicy) {
    if (next === policy || saving) return;
    const before = policy;
    policy = next;
    saving = next;
    error = null;
    try {
      await api.android.powerPolicySet(next);
    } catch (e) {
      policy = before;
      error = toUiError(e);
    } finally {
      saving = null;
    }
  }
</script>

<fieldset class="policies">
  <legend class="visually-hidden">Power policy</legend>
  {#each POLICIES as p (p.value)}
    <label class={["policy", { checked: policy === p.value }]}>
      <input
        class="visually-hidden"
        type="radio"
        name="power-{uid}"
        value={p.value}
        checked={policy === p.value}
        disabled={saving !== null}
        aria-describedby="power-{uid}-{p.value}"
        onchange={() => choose(p.value)}
      />
      <span class="top">
        <span class="radio" aria-hidden="true"></span>
        <span class="title">{p.title}</span>
      </span>
      <span class="body" id="power-{uid}-{p.value}" aria-hidden="true">
        <span class="text">{p.text}</span>
        <span class="cost">{p.cost}</span>
      </span>
    </label>
  {/each}
</fieldset>
{#if error}
  <p class="error" role="alert">
    {error.message}{#if error.hint}<span class="hint">{error.hint}</span>{/if}
  </p>
{/if}

<style>
  .policies {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(100%, 190px), 1fr));
    gap: var(--s-3);
    margin: 0;
    padding: 0;
    border: 0;
    min-width: 0;
  }
  .policy {
    display: grid;
    align-content: start;
    gap: var(--s-2);
    padding: var(--s-4);
    border: 1px solid var(--line);
    border-radius: var(--r-md);
    background: var(--surface);
    cursor: pointer;
    transition:
      border-color var(--dur-1) var(--ease-out),
      background-color var(--dur-1) var(--ease-out);
  }
  .policy:hover {
    border-color: var(--line-strong);
  }
  .policy.checked {
    border-color: var(--signal);
    background: var(--signal-soft);
  }
  .policy:has(input:focus-visible) {
    box-shadow: var(--focus-ring);
  }
  .top {
    display: flex;
    align-items: center;
    gap: var(--s-2);
  }
  .radio {
    width: 16px;
    height: 16px;
    border: 1.5px solid var(--line-strong);
    border-radius: 50%;
    flex: none;
  }
  .checked .radio {
    border: 5px solid var(--signal);
  }
  .title {
    font-weight: var(--fw-semibold);
    color: var(--ink);
  }
  .body {
    display: grid;
    gap: var(--s-2);
    font-size: var(--fs-sm);
  }
  .text {
    color: var(--ink-2);
  }
  .cost {
    color: var(--ink);
    font-weight: var(--fw-medium);
  }
  .error {
    margin-top: var(--s-3);
    font-size: var(--fs-sm);
    color: var(--danger);
  }
  .hint {
    display: block;
    color: var(--ink-2);
  }
</style>
