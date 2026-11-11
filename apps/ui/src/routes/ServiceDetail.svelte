<script lang="ts">
  import { api, toUiError, type PlacementDto, type UiError } from "../lib/api";
  import Button from "../lib/design/Button.svelte";
  import Confirm from "../lib/design/Confirm.svelte";
  import Field from "../lib/design/Field.svelte";
  import Icon from "../lib/design/Icon.svelte";
  import Monogram from "../lib/design/Monogram.svelte";
  import Sheet from "../lib/design/Sheet.svelte";
  import { bytes } from "../lib/format";
  import {
    hostMembers,
    hostsOf,
    kindOf,
    LIMIT_BOUNDS,
    localState,
    MIB,
    placementReady,
    plain,
    samePlacement,
  } from "../lib/service";
  import { members, refreshNow, services, toast } from "../lib/stores";
  import OutboundEditor from "./OutboundEditor.svelte";
  import PlacementEditor from "./PlacementEditor.svelte";

  interface Props {
    name: string;
    open: boolean;
    onclose: () => void;
  }
  let { name, open, onclose }: Props = $props();

  const ALL_HOSTS: PlacementDto = { kind: "allHosts" };

  const svc = $derived($services.find((s) => s.name === name) ?? null);
  const hosts = $derived(svc ? hostsOf(svc, $members) : []);
  const kind = $derived(svc ? kindOf(svc.kind) : null);
  const here = $derived(svc ? localState(svc) : null);
  const candidates = $derived(hostMembers($members));

  // Drafts: `null` means "as deployed", so a poll that changes the service shows through.
  let placement = $state<PlacementDto | null>(null);
  let outbound = $state<string[] | null>(null);
  let memory = $state<string | null>(null);
  let timeout = $state<string | null>(null);
  let body = $state<string | null>(null);

  let busy = $state<"placement" | "outbound" | "limits" | null>(null);
  let errors = $state<Record<"placement" | "outbound" | "limits", UiError | null>>({
    placement: null,
    outbound: null,
    limits: null,
  });
  let confirm = $state<"undeploy" | "purge" | null>(null);

  // A different service, or reopening, starts from what is deployed.
  $effect(() => {
    void name;
    if (!open) return;
    placement = outbound = memory = timeout = body = null;
    errors = { placement: null, outbound: null, limits: null };
  });

  const shownPlacement: PlacementDto = $derived(placement ?? svc?.placement ?? ALL_HOSTS);
  const placementDirty = $derived(
    !!svc && placement !== null && !samePlacement(placement, svc.placement),
  );
  const shownOutbound = $derived(outbound ?? svc?.outbound ?? []);
  const outboundDirty = $derived(
    !!svc && outbound !== null && outbound.join("\n") !== svc.outbound.join("\n"),
  );

  const memoryText = $derived(memory ?? (svc ? String(svc.limits.memoryMb) : ""));
  const timeoutText = $derived(timeout ?? (svc ? plain(svc.limits.requestTimeoutMs / 1000) : ""));
  const bodyText = $derived(body ?? (svc ? plain(svc.limits.maxBodyBytes / MIB) : ""));
  const limitsDirty = $derived(memory !== null || timeout !== null || body !== null);

  let limitErrors = $state<{ memory?: string; timeout?: string; body?: string }>({});

  async function run(section: "placement" | "outbound" | "limits", op: () => Promise<void>) {
    busy = section;
    errors[section] = null;
    try {
      await op();
      await refreshNow();
    } catch (e) {
      errors[section] = toUiError(e);
    } finally {
      busy = null;
    }
  }

  function savePlacement() {
    const p = placement;
    if (!p) return;
    void run("placement", async () => {
      await api.setPlacement(name, p);
      placement = null;
      toast("Placement saved");
    });
  }

  function saveOutbound() {
    const list = outbound;
    if (!list) return;
    void run("outbound", async () => {
      await api.setOutbound(name, list);
      outbound = null;
      toast("Outbound saved");
    });
  }

  function num(text: string): number {
    return /^\s*\d+(\.\d+)?\s*$/.test(text) ? Number(text) : NaN;
  }

  function saveLimits() {
    const [mLo, mHi] = LIMIT_BOUNDS.memoryMb;
    const [tLo, tHi] = LIMIT_BOUNDS.timeoutS;
    const [bLo, bHi] = LIMIT_BOUNDS.bodyMiB;
    const m = num(memoryText);
    const t = num(timeoutText);
    const b = num(bodyText);
    const next: typeof limitErrors = {};
    if (!Number.isInteger(m) || m < mLo || m > mHi)
      next.memory = `Use a whole number, ${mLo} to ${mHi}`;
    if (!(t >= tLo && t <= tHi)) next.timeout = `Use ${tLo} to ${tHi} seconds`;
    if (!(b >= bLo && b <= bHi)) next.body = `Use ${bLo} to ${bHi} MiB`;
    limitErrors = next;
    if (Object.keys(next).length > 0) return;
    void run("limits", async () => {
      await api.setLimits(name, {
        memoryMb: m,
        requestTimeoutMs: Math.round(t * 1000),
        maxBodyBytes: Math.round(b * MIB),
      });
      memory = timeout = body = null;
      toast("Limits saved");
    });
  }

  const purgeText = $derived(
    "A purge hides all of this service's data on every device. " +
      (svc ? `${name} keeps running and starts again from empty. ` : "") +
      "There is no undo.",
  );

  async function undeploy() {
    await api.undeploy(name);
    confirm = null;
    toast(`Undeployed ${name}`);
    await refreshNow();
  }

  async function purge() {
    await api.purge(name);
    confirm = null;
    toast(`Purged ${name}'s data`);
    await refreshNow();
  }
</script>

<Sheet {open} title={name} wide {onclose}>
  {#if svc && kind && here}
    <div class="detail">
      <section class="summary" aria-label="Summary">
        <span class="kind-tile"><Icon name={kind.icon} size={22} /></span>
        <dl class="facts">
          <div>
            <dt>Kind</dt>
            <dd>{kind.label}</dd>
          </div>
          <div>
            <dt>Version</dt>
            <dd class="num">v{svc.version}</dd>
          </div>
          <div>
            <dt>On this device</dt>
            <dd class={["state", here.tone]}>{here.text}</dd>
          </div>
          <div>
            <dt>Data</dt>
            <dd class="num">{bytes(svc.dataBytes)}</dd>
          </div>
        </dl>
        {#if here.detail}<p class="failure mono">{here.detail}</p>{/if}
      </section>

      <section class="block" aria-labelledby="hosts-h">
        <h3 id="hosts-h">Running on</h3>
        {#if hosts.length > 0}
          <ul class="hosts wrap">
            {#each hosts as h (h.id)}
              <li class="host" style:--h={h.hue}>
                <Monogram name={h.name} hue={h.hue} size="sm" />
                <span class="truncate">{h.name}</span>
              </li>
            {/each}
          </ul>
        {:else}
          <p class="warn-text">No host runs it. Change the placement to choose some.</p>
        {/if}
      </section>

      <section class="block" aria-labelledby="placement-h">
        <h3 id="placement-h">Placement</h3>
        <PlacementEditor
          value={shownPlacement}
          onchange={(p) => (placement = p)}
          hosts={candidates}
          service={name}
        />
        {#if errors.placement}
          <p class="error" role="alert">
            {errors.placement.message}{#if errors.placement.hint}<span class="hint"
                >{errors.placement.hint}</span
              >{/if}
          </p>
        {/if}
        <div class="save">
          <Button
            size="sm"
            variant="primary"
            disabled={!placementDirty || !placementReady(shownPlacement)}
            busy={busy === "placement"}
            onclick={savePlacement}>Save placement</Button
          >
        </div>
      </section>

      {#if svc.kind === "wasm"}
        <section class="block" aria-labelledby="outbound-h">
          <h3 id="outbound-h">Outbound connections</h3>
          <p class="help">Addresses outside the mesh that {name} may connect to.</p>
          <OutboundEditor value={shownOutbound} onchange={(l) => (outbound = l)} />
          {#if errors.outbound}
            <p class="error" role="alert">
              {errors.outbound.message}{#if errors.outbound.hint}<span class="hint"
                  >{errors.outbound.hint}</span
                >{/if}
            </p>
          {/if}
          <div class="save">
            <Button
              size="sm"
              variant="primary"
              disabled={!outboundDirty}
              busy={busy === "outbound"}
              onclick={saveOutbound}>Save outbound</Button
            >
          </div>
        </section>

        <section class="block" aria-labelledby="limits-h">
          <h3 id="limits-h">Limits</h3>
          <div class="limits">
            <Field
              label="Memory (MB)"
              inputmode="numeric"
              bind:value={() => memoryText, (x) => (memory = x)}
              error={limitErrors.memory ?? null}
            />
            <Field
              label="Request timeout (s)"
              inputmode="decimal"
              bind:value={() => timeoutText, (x) => (timeout = x)}
              error={limitErrors.timeout ?? null}
            />
            <Field
              label="Largest request (MiB)"
              inputmode="decimal"
              bind:value={() => bodyText, (x) => (body = x)}
              error={limitErrors.body ?? null}
            />
          </div>
          {#if errors.limits}
            <p class="error" role="alert">
              {errors.limits.message}{#if errors.limits.hint}<span class="hint"
                  >{errors.limits.hint}</span
                >{/if}
            </p>
          {/if}
          <div class="save">
            <Button
              size="sm"
              variant="primary"
              disabled={!limitsDirty}
              busy={busy === "limits"}
              onclick={saveLimits}>Save limits</Button
            >
          </div>
        </section>
      {/if}

      <section class="danger" aria-labelledby="danger-h">
        <h3 id="danger-h">Remove</h3>
        <div class="danger-row">
          <p>
            <strong>Undeploy</strong> stops it on every device. Its data stays until you purge it.
          </p>
          <Button onclick={() => (confirm = "undeploy")}>Undeploy…</Button>
        </div>
        <div class="danger-row">
          <p><strong>Purge</strong> hides its data on every device, for good.</p>
          <Button variant="danger" onclick={() => (confirm = "purge")}>Purge data…</Button>
        </div>
      </section>
    </div>
  {:else}
    <div class="gone">
      <p class="gone-title">{name} isn't deployed.</p>
      <p>Any data it left stays on your devices until you purge it.</p>
      <div>
        <Button variant="danger" onclick={() => (confirm = "purge")}>Purge data…</Button>
      </div>
    </div>
  {/if}
</Sheet>

<Confirm
  open={open && confirm === "undeploy"}
  title="Undeploy {name}?"
  consequence="Undeploying stops {name} on every device. Its data stays on your devices until you purge it."
  typeToConfirm={name}
  actionLabel="Undeploy"
  onConfirm={undeploy}
  onCancel={() => (confirm = null)}
/>
<Confirm
  open={open && confirm === "purge"}
  title="Purge {name}'s data?"
  consequence={purgeText}
  typeToConfirm={name}
  actionLabel="Purge data"
  onConfirm={purge}
  onCancel={() => (confirm = null)}
/>

<style>
  .detail {
    display: grid;
    gap: var(--s-6);
  }

  .summary {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    gap: var(--s-2) var(--s-4);
    align-items: center;
    padding: var(--s-4);
    border-radius: var(--r-md);
    background: var(--surface-2);
  }
  .kind-tile {
    display: grid;
    place-items: center;
    width: 44px;
    height: 44px;
    border-radius: var(--r-sm);
    background: var(--surface);
    color: var(--ink-2);
    box-shadow: var(--e-1);
  }
  .facts {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(110px, 1fr));
    gap: var(--s-2) var(--s-4);
    margin: 0;
  }
  dt {
    font-size: var(--fs-xs);
    color: var(--ink-3);
  }
  dd {
    margin: 0;
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
  }
  .state.ok {
    color: var(--ok);
  }
  .state.warn {
    color: var(--warn);
  }
  .state.danger {
    color: var(--danger);
  }
  .state.quiet {
    color: var(--ink-2);
  }
  .failure {
    grid-column: 1 / -1;
    color: var(--danger);
    font-size: var(--fs-xs);
    overflow-wrap: anywhere;
  }

  .block {
    display: grid;
    gap: var(--s-3);
    min-width: 0;
  }
  h3 {
    font-size: var(--fs-md);
  }
  .help {
    margin-top: calc(var(--s-2) * -1);
    font-size: var(--fs-sm);
    color: var(--ink-2);
  }

  .hosts {
    gap: var(--s-2);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .host {
    display: inline-flex;
    align-items: center;
    gap: var(--s-2);
    max-width: 100%;
    padding: 4px var(--s-3) 4px 4px;
    border-radius: var(--r-pill);
    background: var(--accent-soft);
    color: var(--accent-ink);
    font-size: var(--fs-sm);
    font-weight: var(--fw-medium);
  }
  .host :global(.mono-tile) {
    border-radius: 50%;
    background: var(--surface);
  }
  .warn-text {
    font-size: var(--fs-sm);
    color: var(--warn);
  }

  .limits {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
    gap: var(--s-3);
  }

  .save {
    display: flex;
    justify-content: flex-end;
  }
  .error {
    font-size: var(--fs-sm);
    color: var(--danger);
  }
  .hint {
    display: block;
    color: var(--ink-2);
  }

  /* The destructive actions, set apart at the end. */
  .danger {
    display: grid;
    gap: var(--s-3);
    padding-top: var(--s-5);
    border-top: 1px solid var(--line);
  }
  .danger-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--s-4);
  }
  .danger-row p {
    font-size: var(--fs-sm);
    color: var(--ink-2);
  }
  .danger-row strong {
    color: var(--ink);
    font-weight: var(--fw-semibold);
  }

  .gone {
    display: grid;
    gap: var(--s-3);
    color: var(--ink-2);
  }
  .gone-title {
    color: var(--ink);
    font-size: var(--fs-lg);
    font-weight: var(--fw-semibold);
  }

  @media (max-width: 639px) {
    .danger-row {
      flex-direction: column;
      align-items: stretch;
    }
  }
</style>
