<script lang="ts">
  import { onMount } from "svelte";
  import { api, toUiError, type Platform, type UiError } from "../lib/api";
  import Button from "../lib/design/Button.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import { copyText } from "../lib/device";
  import { navigate } from "../lib/router";
  import { banner, refreshNow, status, toast } from "../lib/stores";

  /** Where `meshd run` writes its log: `<user dir>/meshd.log` (apps/desktop `launch.rs`). */
  const LOG_PATH: Record<Exclude<Platform, "android">, string> = {
    "desktop-windows": String.raw`%LOCALAPPDATA%\Meshnode\meshd.log`,
    "desktop-macos": "~/Library/Application Support/Meshnode/meshd.log",
    "desktop-linux": "~/.local/share/meshnode/meshd.log",
  };

  type Phase = "idle" | "starting" | "installing" | "waiting";
  interface Problem {
    title: string;
    detail: string | null;
    hint: string | null;
    log?: string;
  }

  let platform: Platform | null = $state(null);
  /** The `meshd` found next to the app or on PATH; `undefined` while looking. */
  let meshd: string | null | undefined = $state(undefined);
  let phase: Phase = $state("idle");
  let problem: Problem | null = $state(null);

  const busy = $derived(phase !== "idle");
  const logPath = $derived(
    LOG_PATH[platform && platform !== "android" ? platform : "desktop-windows"],
  );

  // A daemon answered (started here or elsewhere): this screen is done.
  $effect(() => {
    if ($status !== null && $banner === null) navigate({ name: "home" }, { replace: true });
  });

  async function locate() {
    try {
      meshd = await api.desktop.locateMeshd();
    } catch {
      meshd = null;
    }
  }

  /** Why the daemon last refused to start (incompatible data), when it left that word. */
  let startError: string | null = $state(null);
  /** The service is installed but this account may not reach it (Linux: not in the group). */
  let noAccess = $state(false);
  /** An action's problem first; otherwise the daemon's own refusal; otherwise no access. */
  const shown: Problem | null = $derived(
    problem ??
      (startError
        ? { title: "Meshnode refused to start", detail: startError, hint: null }
        : noAccess
          ? {
              title: "This account can't reach the Meshnode service",
              detail: "The service is installed, but your account isn't in the meshnode group yet.",
              hint: "Join it once with sudo usermod -aG meshnode $USER, then log in again.",
            }
          : null),
  );

  async function loadStartError() {
    try {
      ({ startError, noAccess } = await api.desktop.connection());
    } catch {
      // No connection info: the screen works without it.
    }
  }

  // Each failed status poll (the banner is set again) re-reads it, so a refusal that
  // happens while this screen is open shows up, and a cleared one goes away.
  $effect(() => {
    if ($banner !== null) void loadStartError();
  });

  onMount(() => {
    api
      .platform()
      .then((p) => (platform = p))
      .catch(() => {});
    void locate();
  });

  async function runUser() {
    if (busy) return;
    phase = "starting";
    problem = null;
    try {
      await api.desktop.daemonRunUser();
      phase = "waiting";
      await refreshNow();
    } catch (e) {
      const err = toUiError(e);
      problem =
        err.code === "daemonStarting"
          ? {
              title: "Meshnode didn't start",
              detail: "It was started but hasn't answered. Its log says why:",
              hint: null,
              log: logPath,
            }
          : { title: "Couldn't start Meshnode", detail: err.message, hint: err.hint };
    } finally {
      if (phase !== "waiting" || $banner !== null) phase = "idle";
    }
  }

  async function install() {
    if (busy) return;
    phase = "installing";
    problem = null;
    try {
      await api.desktop.daemonInstall();
      phase = "waiting";
      await refreshNow();
    } catch (e) {
      const err: UiError = toUiError(e);
      problem =
        err.code === "cancelled"
          ? { title: "Installation cancelled", detail: null, hint: null }
          : err.code === "daemonStarting"
            ? {
                title: "Meshnode didn't start",
                detail: "The service was installed but hasn't answered yet. Try again in a moment.",
                hint: null,
              }
            : { title: "Couldn't install the service", detail: err.message, hint: err.hint };
    } finally {
      if (phase !== "waiting" || $banner !== null) phase = "idle";
    }
  }

  async function locateMeshd() {
    problem = null;
    try {
      if ((await api.desktop.locateMeshdDialog()) === null) return;
      await locate();
    } catch (e) {
      const err = toUiError(e);
      problem = { title: "Couldn't use that file", detail: err.message, hint: err.hint };
    }
  }

  async function copyLog() {
    if (await copyText(logPath)) toast("Log path copied");
  }
</script>

<section class="no-daemon">
  <div class="hero">
    <span class="mark" aria-hidden="true"><Icon name="mesh" size={28} /></span>
    <h1>Meshnode isn't running on this computer</h1>
    <p class="lede">
      This app manages Meshnode but doesn't run it. Start it here, or install it so it starts with
      the computer.
    </p>
  </div>

  <div class="options">
    <div class="option primary">
      <div class="text">
        <h2>Run without installing</h2>
        <p>
          Starts Meshnode as you, now. It keeps running after you close this app, until you sign out
          or restart.
        </p>
      </div>
      <Button
        variant="primary"
        onclick={runUser}
        disabled={busy || !meshd}
        busy={phase === "starting"}>Run without installing</Button
      >
    </div>
    <div class="option">
      <div class="text">
        <h2>Install as a service</h2>
        <p>Starts with the computer, for every user. Asks for administrator permission.</p>
      </div>
      <Button onclick={install} disabled={busy || !meshd} busy={phase === "installing"}
        >Install as a service</Button
      >
    </div>
  </div>

  {#if phase === "starting" || phase === "waiting"}
    <p class="progress" role="status">
      <span class="spinner" aria-hidden="true"></span>Starting…
    </p>
  {:else if phase === "installing"}
    <p class="progress" role="status">
      <span class="spinner" aria-hidden="true"></span>Installing… Approve the permission prompt to
      continue.
    </p>
  {/if}

  {#if shown}
    <div class="problem" role="alert">
      <Icon name="alert" size={18} />
      <div>
        <strong>{shown.title}</strong>
        {#if shown.detail}<p>{shown.detail}</p>{/if}
        {#if shown.log}
          <p class="log">
            <code>{shown.log}</code>
            <Button size="sm" variant="ghost" onclick={copyLog}
              ><Icon name="copy" size={14} />Copy path</Button
            >
          </p>
        {/if}
        {#if shown.hint}<p class="hint">{shown.hint}</p>{/if}
      </div>
    </div>
  {/if}

  <footer class="meshd">
    {#if meshd === undefined}
      <p>Looking for meshd…</p>
    {:else if meshd === null}
      <p>
        <Icon name="alert" size={16} />
        <span>meshd wasn't found next to this app or on your PATH. Choose it to continue.</span>
      </p>
    {:else}
      <p>
        <Icon name="file" size={16} />
        <span>Using <code class="truncate" title={meshd}>{meshd}</code></span>
      </p>
    {/if}
    <Button
      size="sm"
      variant={meshd === null ? "primary" : "ghost"}
      onclick={locateMeshd}
      disabled={busy}>Locate meshd…</Button
    >
  </footer>
</section>

<style>
  .no-daemon {
    display: grid;
    gap: var(--s-6);
    max-width: 640px;
    margin: 0 auto;
    padding-top: clamp(var(--s-6), 8vh, var(--s-16));
  }
  .hero {
    display: grid;
    gap: var(--s-3);
  }
  .mark {
    display: grid;
    place-items: center;
    width: 52px;
    height: 52px;
    border-radius: var(--r-md);
    background: var(--surface-2);
    color: var(--ink-3);
  }
  h1 {
    font-size: var(--fs-3xl);
    letter-spacing: -0.025em;
    max-width: 18ch;
  }
  .lede {
    color: var(--ink-2);
    max-width: 52ch;
  }

  .options {
    display: grid;
    border: 1px solid var(--line);
    border-radius: var(--r-md);
    background: var(--surface);
    box-shadow: var(--e-1);
    overflow: hidden;
  }
  .option {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--s-4);
    padding: var(--s-5);
  }
  .option + .option {
    border-top: 1px solid var(--line);
  }
  .option .text {
    display: grid;
    gap: var(--s-1);
    min-width: 0;
  }
  .option h2 {
    font-size: var(--fs-md);
  }
  .option p {
    color: var(--ink-3);
    font-size: var(--fs-sm);
    max-width: 44ch;
  }
  .option > :global(.btn) {
    flex: none;
    min-width: 200px;
  }

  .progress {
    display: flex;
    align-items: center;
    gap: var(--s-2);
    color: var(--ink-2);
    font-size: var(--fs-sm);
  }
  .spinner {
    width: 14px;
    height: 14px;
    border: 2px solid var(--signal);
    border-right-color: transparent;
    border-radius: 50%;
    animation: spin 800ms linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  .problem {
    display: flex;
    gap: var(--s-3);
    padding: var(--s-4);
    border-radius: var(--r-sm);
    background: var(--danger-soft);
    color: var(--ink);
    font-size: var(--fs-sm);
  }
  .problem > :global(.icon) {
    flex: none;
    color: var(--danger);
  }
  .problem > div {
    display: grid;
    gap: var(--s-1);
    min-width: 0;
  }
  .problem p {
    color: var(--ink-2);
  }
  .log {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--s-2);
  }
  .log code {
    overflow-wrap: anywhere;
    color: var(--ink);
  }

  .meshd {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: var(--s-2) var(--s-4);
    color: var(--ink-3);
    font-size: var(--fs-sm);
  }
  .meshd p {
    display: flex;
    align-items: center;
    gap: var(--s-2);
    min-width: 0;
  }
  .meshd p > span {
    min-width: 0;
  }
  .meshd code {
    display: inline-block;
    max-width: 42ch;
    vertical-align: bottom;
    color: var(--ink-2);
  }

  @media (max-width: 639px) {
    h1 {
      font-size: var(--fs-2xl);
    }
    .option {
      flex-direction: column;
      align-items: stretch;
    }
    .option > :global(.btn) {
      min-width: 0;
    }
  }
</style>
