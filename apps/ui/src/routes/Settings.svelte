<script lang="ts">
  import { onMount } from "svelte";
  import { api, toUiError, type ConnectionDto, type Platform, type UiError } from "../lib/api";
  import Button from "../lib/design/Button.svelte";
  import Chip from "../lib/design/Chip.svelte";
  import Field from "../lib/design/Field.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import { NAME_RULE, validServiceName } from "../lib/service";
  import { members, refreshNow, status, toast } from "../lib/stores";
  import { theme, type Theme } from "../lib/theme";
  import PowerPolicy from "./PowerPolicy.svelte";
  import ProbeChart from "./ProbeChart.svelte";
  import ResetIdentity from "./ResetIdentity.svelte";

  /** The daemon's limits on this device's info (mesh-proto `join.rs`). */
  const MAX_NAME_BYTES = 64;
  const MAX_TAGS = 16;
  const MAX_TAG_BYTES = 32;
  const bytesOf = (s: string) => new TextEncoder().encode(s).length;

  const uid = $props.id();
  let platform: Platform | null = $state(null);
  const android = $derived(platform === "android");
  let connection: ConnectionDto | null = $state(null);

  // ---- This device: name and tags --------------------------------------------------------
  let name = $state("");
  let tags: string[] = $state([]);
  let newTag = $state("");
  let infoDirty = $state(false);
  let infoBusy = $state(false);
  let infoError: UiError | null = $state(null);
  let tagError: string | null = $state(null);

  // Until the user edits, the fields follow what the daemon reports.
  $effect(() => {
    const s = $status;
    if (!infoDirty && s) {
      name = s.name ?? "";
      tags = [...s.tags];
    }
  });

  function addTag(e?: SubmitEvent) {
    e?.preventDefault();
    const t = newTag.trim();
    tagError = null;
    if (!t) return;
    if (/[,\s]/.test(t)) {
      tagError = "Use one word per tag, without commas";
      return;
    }
    if (bytesOf(t) > MAX_TAG_BYTES) {
      tagError = `Keep a tag to ${MAX_TAG_BYTES} characters`;
      return;
    }
    if (!tags.includes(t)) {
      if (tags.length >= MAX_TAGS) {
        tagError = `A device can have up to ${MAX_TAGS} tags`;
        return;
      }
      tags = [...tags, t];
      infoDirty = true;
    }
    newTag = "";
  }

  function removeTag(t: string) {
    tags = tags.filter((x) => x !== t);
    infoDirty = true;
  }

  const nameTrimmed = $derived(name.trim());
  const nameProblem = $derived(
    nameTrimmed === ""
      ? "Give this device a name"
      : bytesOf(nameTrimmed) > MAX_NAME_BYTES
        ? `Keep the name to ${MAX_NAME_BYTES} characters`
        : null,
  );

  async function saveInfo(e?: SubmitEvent) {
    e?.preventDefault();
    if (nameProblem || infoBusy) return;
    infoBusy = true;
    infoError = null;
    try {
      await api.setInfo({ name: nameTrimmed, tags: [...tags] });
      infoDirty = false;
      await refreshNow();
      toast("Device saved");
    } catch (err) {
      infoError = toUiError(err);
    } finally {
      infoBusy = false;
    }
  }

  // ---- Config: port mappings and relay -------------------------------------------------
  interface Mapping {
    key: number;
    port: string;
    service: string;
  }
  let configError: UiError | null = $state(null);
  let listenPort: number | null = $state(null);
  let mappingSeq = 0;
  let mappings: Mapping[] = $state([]);
  let portsBusy = $state(false);
  let portsError: { message: string; hint: string | null } | null = $state(null);
  let portsRestart = $state(false);

  type RelayMode = "default" | "disabled" | "custom";
  let relayMode: RelayMode = $state("default");
  let relayUrl = $state("");
  let relayBusy = $state(false);
  let relayError: { message: string; hint: string | null } | null = $state(null);
  let relayRestart = $state(false);

  function parseMappings(value: string): Mapping[] {
    return value
      .split(",")
      .map((e) => e.trim())
      .filter(Boolean)
      .map((e) => {
        const [port = "", service = ""] = e.split("=");
        return { key: mappingSeq++, port: port.trim(), service: service.trim() };
      });
  }

  async function loadConfig() {
    try {
      const entries = await api.configGet();
      const get = (k: string) => entries.find((e) => e.key === k)?.value ?? "";
      mappings = parseMappings(get("gateway.ports"));
      const listen = /:(\d+)$/.exec(get("gateway.listen"));
      listenPort = listen ? Number(listen[1]) : null;
      const relay = get("relay");
      if (relay === "disabled") relayMode = "disabled";
      else if (relay.startsWith("https://")) {
        relayMode = "custom";
        relayUrl = relay;
      } else relayMode = "default";
      configError = null;
    } catch (e) {
      configError = toUiError(e);
    }
  }

  /** The mappings in `config set`'s format ("7801=sync,9000=web"), or the first problem. */
  function mappingsValue(): { value: string } | { problem: string } {
    const seen: number[] = [];
    const out: string[] = [];
    for (const m of mappings) {
      const port = m.port.trim();
      const service = m.service.trim();
      if (port === "" && service === "") continue;
      const n = Number(port);
      if (!/^\d{1,5}$/.test(port) || n < 1024 || n > 65535) {
        return { problem: "Use a port from 1024 to 65535" };
      }
      if (n === listenPort) return { problem: `${n} is the main gateway port` };
      if (seen.includes(n)) return { problem: `Port ${n} is mapped twice` };
      if (!validServiceName(service)) return { problem: `${NAME_RULE} for the service` };
      seen.push(n);
      out.push(`${n}=${service}`);
    }
    return { value: out.join(",") };
  }

  function addMapping() {
    mappings = [...mappings, { key: mappingSeq++, port: "", service: "" }];
  }

  function removeMapping(key: number) {
    mappings = mappings.filter((m) => m.key !== key);
  }

  async function savePorts(e?: SubmitEvent) {
    e?.preventDefault();
    if (portsBusy) return;
    const v = mappingsValue();
    if ("problem" in v) {
      portsError = { message: v.problem, hint: null };
      return;
    }
    portsBusy = true;
    portsError = null;
    try {
      const r = await api.configSet("gateway.ports", v.value);
      portsRestart = r.restartRequired;
      toast("Port mappings saved");
    } catch (err) {
      const u = toUiError(err);
      portsError = { message: u.message, hint: u.hint };
    } finally {
      portsBusy = false;
    }
  }

  async function saveRelay(e?: SubmitEvent) {
    e?.preventDefault();
    if (relayBusy) return;
    const url = relayUrl.trim();
    if (relayMode === "custom" && !/^https:\/\/\S+$/.test(url)) {
      relayError = { message: "Use an https:// address", hint: null };
      return;
    }
    const value = relayMode === "custom" ? url : relayMode;
    relayBusy = true;
    relayError = null;
    try {
      const r = await api.configSet("relay", value);
      relayRestart = r.restartRequired;
      toast("Relay saved");
    } catch (err) {
      const u = toUiError(err);
      relayError = { message: u.message, hint: u.hint };
    } finally {
      relayBusy = false;
    }
  }

  // ---- Appearance -----------------------------------------------------------------------
  const THEMES: { value: Theme; label: string }[] = [
    { value: "system", label: "System" },
    { value: "light", label: "Light" },
    { value: "dark", label: "Dark" },
  ];

  // ---- Latency --------------------------------------------------------------------------
  const others = $derived(
    $members
      .filter((m) => !m.isSelf)
      .sort((a, b) => Number(b.online) - Number(a.online) || a.name.localeCompare(b.name)),
  );

  // ---- Danger zone ----------------------------------------------------------------------
  const removed = $derived($status?.state === "removed");

  onMount(() => {
    void loadConfig();
    api
      .platform()
      .then((p) => {
        platform = p;
        if (p !== "android") {
          api.desktop
            .connection()
            .then((c) => (connection = c))
            .catch(() => (connection = null));
        }
      })
      .catch(() => (platform = null));
  });

  const CONNECTION: Record<ConnectionDto["kind"], string> = {
    system: "System service",
    user: "Running as you",
    none: "Not connected",
  };
</script>

<div class="settings">
  <header class="page-head">
    <h1>Settings</h1>
  </header>

  <!-- This device -->
  <section class="group" aria-labelledby="{uid}-device">
    <div class="intro">
      <h2 id="{uid}-device">This device</h2>
      <p>How this device appears to the others. Tags decide which services it runs.</p>
    </div>
    <form class="panel" onsubmit={saveInfo}>
      <Field
        label="Device name"
        bind:value={name}
        oninput={() => (infoDirty = true)}
        autocomplete="off"
        error={infoDirty ? nameProblem : null}
      />
      <div class="tags-field">
        <span class="label" id="{uid}-tags">Tags</span>
        {#if tags.length > 0}
          <div class="tags wrap" role="list" aria-labelledby="{uid}-tags">
            {#each tags as t (t)}
              <span role="listitem"><Chip label={t} onremove={() => removeTag(t)} /></span>
            {/each}
          </div>
        {:else}
          <p class="none">No tags yet.</p>
        {/if}
        <div class="add-tag">
          <Field
            label="Add a tag"
            bind:value={newTag}
            autocomplete="off"
            autocapitalize="off"
            spellcheck={false}
            placeholder="e.g. office"
            error={tagError}
            onkeydown={(e: KeyboardEvent) => {
              if (e.key === "Enter") {
                e.preventDefault();
                addTag();
              }
            }}
          />
          <Button aria-label="Add tag" onclick={() => addTag()}>
            <Icon name="plus" size={16} />Add
          </Button>
        </div>
      </div>
      {#if infoError}
        <p class="error" role="alert">
          {infoError.message}{#if infoError.hint}<span class="hint">{infoError.hint}</span>{/if}
        </p>
      {/if}
      <div class="actions">
        <Button variant="primary" type="submit" busy={infoBusy} disabled={!!nameProblem}
          >Save device</Button
        >
      </div>
      {#if connection}
        <div class="connection">
          <span class="label">Connected to</span>
          <span class="value">{CONNECTION[connection.kind]}</span>
          {#if connection.otherRunning}
            <p class="note">
              <Icon name="info" size={16} />
              <span
                >A second, separate node is running as you on this computer. This app shows the
                system service.</span
              >
            </p>
          {/if}
        </div>
      {/if}
    </form>
  </section>

  <!-- Port mappings -->
  <section class="group" aria-labelledby="{uid}-ports">
    <div class="intro">
      <h2 id="{uid}-ports">Port mappings</h2>
      <p>
        Give a service its own local port, for apps that can't use a
        <span class="mono">name.localhost</span> address.
      </p>
    </div>
    <form class="panel" onsubmit={savePorts}>
      {#if configError}
        <p class="error" role="alert">
          Couldn't read the settings: {configError.message}{#if configError.hint}<span class="hint"
              >{configError.hint}</span
            >{/if}
        </p>
      {/if}
      {#if mappings.length > 0}
        <ul class="mappings">
          {#each mappings as m, i (m.key)}
            <li class="mapping">
              <label class="cell port">
                <span class="prefix mono" aria-hidden="true">127.0.0.1:</span>
                <input
                  class="mono num"
                  aria-label="Port {i + 1}"
                  inputmode="numeric"
                  autocomplete="off"
                  placeholder="9000"
                  bind:value={m.port}
                />
              </label>
              <Icon name="chevron" size={16} />
              <label class="cell service">
                <input
                  class="mono"
                  aria-label="Service {i + 1}"
                  autocomplete="off"
                  autocapitalize="off"
                  spellcheck="false"
                  placeholder="service"
                  bind:value={m.service}
                />
              </label>
              <button
                type="button"
                class="remove-port"
                aria-label="Remove mapping {i + 1}"
                onclick={() => removeMapping(m.key)}
              >
                <Icon name="close" size={16} />
              </button>
            </li>
          {/each}
        </ul>
      {:else}
        <p class="none">No ports mapped. Services are still at their own address.</p>
      {/if}
      {#if portsError}
        <p class="error" role="alert">
          {portsError.message}{#if portsError.hint}<span class="hint">{portsError.hint}</span>{/if}
        </p>
      {/if}
      <div class="actions spread">
        <Button variant="ghost" onclick={addMapping}>
          <Icon name="plus" size={16} />Add a mapping
        </Button>
        <Button type="submit" busy={portsBusy}>Save port mappings</Button>
      </div>
      {#if portsRestart}
        <p class="restart" role="status">
          <Icon name="refresh" size={16} /><span>Restart Meshnode to apply</span>
        </p>
      {/if}
    </form>
  </section>

  <!-- Relay -->
  <section class="group" aria-labelledby="{uid}-relay">
    <div class="intro">
      <h2 id="{uid}-relay">Relay</h2>
      <p>When two devices can't reach each other directly, they talk through a relay.</p>
    </div>
    <form class="panel" onsubmit={saveRelay}>
      <fieldset class="segments">
        <legend class="visually-hidden">Relay</legend>
        {#each [{ v: "default", l: "Default" }, { v: "disabled", l: "Off" }, { v: "custom", l: "Custom" }] as o (o.v)}
          <label class={["segment", { checked: relayMode === o.v }]}>
            <input
              class="visually-hidden"
              type="radio"
              name="{uid}-relay-mode"
              value={o.v}
              bind:group={relayMode}
            />
            {o.l}
          </label>
        {/each}
      </fieldset>
      <p class="help">
        {relayMode === "default"
          ? "Uses the public relays. They pass traffic along but can't read it."
          : relayMode === "disabled"
            ? "Devices connect only directly. Some may not reach each other."
            : "Uses a relay you run."}
      </p>
      {#if relayMode === "custom"}
        <Field
          label="Relay URL"
          mono
          bind:value={relayUrl}
          placeholder="https://relay.example"
          autocomplete="off"
          autocapitalize="off"
          spellcheck={false}
        />
      {/if}
      {#if relayError}
        <p class="error" role="alert">
          {relayError.message}{#if relayError.hint}<span class="hint">{relayError.hint}</span>{/if}
        </p>
      {/if}
      <div class="actions">
        <Button type="submit" busy={relayBusy}>Save relay</Button>
      </div>
      {#if relayRestart}
        <p class="restart" role="status">
          <Icon name="refresh" size={16} /><span>Restart Meshnode to apply</span>
        </p>
      {/if}
    </form>
  </section>

  <!-- Appearance -->
  <section class="group" aria-labelledby="{uid}-look">
    <div class="intro">
      <h2 id="{uid}-look">Appearance</h2>
      <p>System follows your device's light or dark setting.</p>
    </div>
    <div class="panel">
      <fieldset class="segments">
        <legend class="visually-hidden">Theme</legend>
        {#each THEMES as t (t.value)}
          <label class={["segment", { checked: $theme === t.value }]}>
            <input
              class="visually-hidden"
              type="radio"
              name="{uid}-theme"
              value={t.value}
              checked={$theme === t.value}
              onchange={() => theme.set(t.value)}
            />
            {t.label}
          </label>
        {/each}
      </fieldset>
    </div>
  </section>

  {#if android}
    <!-- Battery (Android) -->
    <section class="group" aria-labelledby="{uid}-battery">
      <div class="intro">
        <h2 id="{uid}-battery">Battery</h2>
        <p>How hard this phone works to stay reachable. You can change it any time.</p>
      </div>
      <div class="panel bare">
        <PowerPolicy />
      </div>
    </section>
  {/if}

  <!-- Latency -->
  <section class="group" aria-labelledby="{uid}-latency">
    <div class="intro">
      <h2 id="{uid}-latency">Latency</h2>
      <p>Measure the round trip to another device, live.</p>
    </div>
    <div class="panel">
      <ProbeChart devices={others} />
    </div>
  </section>

  <!-- Danger zone -->
  <section class="group danger" aria-labelledby="{uid}-danger">
    <div class="intro">
      <h2 id="{uid}-danger">Danger zone</h2>
      <p>Actions here can't be undone.</p>
    </div>
    <div class="panel">
      <div class="danger-row">
        <div class="danger-text">
          <h3>Reset identity</h3>
          {#if removed}
            <p>
              This device was removed from its mesh. Reset it to start again: you can then create a
              new mesh or join one with a new invite.
            </p>
          {:else}
            <p>
              Only a device that was removed can be reset. To take this device out of the mesh,
              remove it from another device's Home screen first.
            </p>
          {/if}
        </div>
        <ResetIdentity disabled={!removed} />
      </div>
    </div>
  </section>
</div>

<style>
  .settings {
    display: grid;
    gap: var(--s-2);
  }
  .page-head {
    margin-bottom: var(--s-4);
  }

  /* Each setting: what it is on the left, the controls on the right; stacked on phones. */
  .group {
    display: grid;
    grid-template-columns: minmax(180px, 280px) minmax(0, 1fr);
    gap: var(--s-4) var(--s-8);
    padding: var(--s-6) 0;
    border-top: 1px solid var(--line);
  }
  .intro {
    display: grid;
    align-content: start;
    gap: var(--s-1);
  }
  .intro h2 {
    font-size: var(--fs-lg);
  }
  .intro p {
    color: var(--ink-3);
    font-size: var(--fs-sm);
  }
  .panel {
    display: grid;
    gap: var(--s-4);
    min-width: 0;
    padding: var(--s-5);
    border: 1px solid var(--line);
    border-radius: var(--r-md);
    background: var(--surface);
    box-shadow: var(--e-1);
  }
  .panel.bare {
    padding: 0;
    border: 0;
    background: transparent;
    box-shadow: none;
  }

  .label {
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    color: var(--ink-2);
  }
  .none,
  .help {
    font-size: var(--fs-sm);
    color: var(--ink-3);
  }
  .tags-field {
    display: grid;
    gap: var(--s-2);
  }
  .tags {
    gap: var(--s-2);
  }
  .add-tag {
    display: flex;
    align-items: flex-start;
    gap: var(--s-2);
  }
  .add-tag :global(.field) {
    flex: 1;
    min-width: 0;
  }
  /* Line the button up with the input, below the field's label. */
  .add-tag > :global(.btn) {
    margin-top: calc(var(--fs-sm) * 1.5 + var(--s-2));
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--s-3);
  }
  .actions.spread {
    justify-content: space-between;
  }

  .connection {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--s-1) var(--s-3);
    padding-top: var(--s-4);
    border-top: 1px solid var(--line);
  }
  .connection .value {
    font-weight: var(--fw-semibold);
  }
  .note {
    display: flex;
    gap: var(--s-2);
    width: 100%;
    padding: var(--s-3);
    border-radius: var(--r-sm);
    background: var(--signal-soft);
    color: var(--ink);
    font-size: var(--fs-sm);
  }
  .note > :global(.icon) {
    flex: none;
    color: var(--signal);
  }

  .mappings {
    display: grid;
    gap: var(--s-2);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .mapping {
    display: flex;
    align-items: center;
    gap: var(--s-2);
    min-width: 0;
  }
  .mapping > :global(.icon) {
    flex: none;
    color: var(--ink-3);
  }
  .cell {
    display: flex;
    align-items: center;
    min-width: 0;
    min-height: var(--touch);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    background: var(--surface);
  }
  .cell:focus-within {
    border-color: var(--signal);
    box-shadow: 0 0 0 3px var(--signal-soft);
  }
  .cell.port {
    flex: 0 1 190px;
  }
  .cell.service {
    flex: 1 1 0;
  }
  .prefix {
    padding-left: var(--s-3);
    color: var(--ink-3);
    font-size: var(--fs-sm);
  }
  .cell input {
    flex: 1;
    width: 100%;
    min-width: 0;
    height: 100%;
    padding: 0 var(--s-3);
    border: 0;
    background: transparent;
    font-size: var(--fs-sm);
  }
  .port input {
    padding-left: 1px;
  }
  .cell input:focus-visible {
    box-shadow: none;
  }
  .remove-port {
    display: grid;
    place-items: center;
    flex: none;
    width: 36px;
    height: 36px;
    border: 0;
    border-radius: var(--r-sm);
    background: transparent;
    color: var(--ink-3);
    cursor: pointer;
  }
  .remove-port:hover {
    background: var(--surface-2);
    color: var(--danger);
  }

  .segments {
    display: inline-flex;
    justify-self: start;
    gap: 2px;
    margin: 0;
    padding: 3px;
    border: 0;
    border-radius: var(--r-sm);
    background: var(--surface-2);
  }
  .segment {
    display: grid;
    place-items: center;
    min-width: 76px;
    min-height: 34px;
    padding: 0 var(--s-4);
    border-radius: calc(var(--r-sm) - 2px);
    color: var(--ink-2);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
    cursor: pointer;
    transition:
      background-color var(--dur-1) var(--ease-out),
      color var(--dur-1) var(--ease-out);
  }
  .segment:hover {
    color: var(--ink);
  }
  .segment.checked {
    background: var(--surface);
    color: var(--ink);
    box-shadow: var(--e-1);
  }
  .segment:has(input:focus-visible) {
    box-shadow: var(--focus-ring);
  }

  .restart {
    display: flex;
    align-items: center;
    gap: var(--s-2);
    padding: var(--s-2) var(--s-3);
    border-radius: var(--r-sm);
    background: var(--warn-soft);
    color: var(--ink);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
  }
  .restart > :global(.icon) {
    color: var(--warn);
  }

  .error {
    font-size: var(--fs-sm);
    color: var(--danger);
  }
  .hint {
    display: block;
    color: var(--ink-2);
  }

  .danger .intro h2 {
    color: var(--danger);
  }
  .danger .panel {
    border-color: color-mix(in oklab, var(--danger) 45%, var(--line));
  }
  .danger-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--s-4);
  }
  .danger-text {
    display: grid;
    gap: var(--s-1);
    max-width: 60ch;
  }
  .danger-text h3 {
    font-size: var(--fs-md);
  }
  .danger-text p {
    color: var(--ink-2);
    font-size: var(--fs-sm);
  }

  @media (pointer: coarse) {
    .segment {
      min-height: var(--touch);
    }
    .remove-port {
      width: var(--touch);
      height: var(--touch);
    }
  }
  @media (max-width: 899px) {
    .group {
      grid-template-columns: minmax(0, 1fr);
      gap: var(--s-3);
    }
  }
  @media (max-width: 479px) {
    .panel {
      padding: var(--s-4);
    }
    .danger-row {
      flex-direction: column;
      align-items: stretch;
    }
    .prefix {
      display: none;
    }
    .cell.port {
      flex: 0 0 96px;
    }
    .actions.spread {
      flex-wrap: wrap;
    }
  }
</style>
