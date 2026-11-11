// The deploy in flight, kept outside any component: closing the Deploy sheet, starting over
// or leaving the Services screen must not lose its progress or its outcome.
import { get, writable } from "svelte/store";
import { api, toUiError, type DeployOptsDto, type Platform, type UiError } from "./api";
import type { ServiceKindDto } from "./generated/ServiceKindDto";
import { refreshNow, toast } from "./stores";

export interface DeployRequest {
  name: string;
  kind: ServiceKindDto;
  /** Desktop path, Android tree URI (sites) or cached file path (wasm). */
  path: string;
  opts: DeployOptsDto;
  platform: Platform | null;
}

export interface DeployState extends DeployRequest {
  /** Bytes read so far (sites). */
  bytes: number;
  status: "sending" | "done" | "failed";
  error?: UiError;
}

/** `null` when no deploy is running and no outcome is waiting to be seen. */
export const deployState = writable<DeployState | null>(null);

let run = 0;
/** Open Deploy sheets: with one open the sheet shows "done", otherwise a toast does. */
let watchers = 0;

/** Registers an open Deploy sheet; returns its release. */
export function watchDeploy(): () => void {
  watchers++;
  return () => {
    watchers--;
  };
}

export function deploying(): boolean {
  return get(deployState)?.status === "sending";
}

/**
 * Starts a deploy unless one is already sending (then `false`). Success ends in "done" for an
 * open sheet, or a toast; failure stays in the store until dismissed, so it is shown inline
 * (on the Services list and in the Deploy sheet) and never lost.
 */
export function startDeploy(req: DeployRequest): boolean {
  if (deploying()) return false;
  const id = ++run;
  deployState.set({ ...req, bytes: 0, status: "sending" });
  const onProgress = (n: number) => {
    if (id === run) deployState.update((d) => (d ? { ...d, bytes: n } : d));
  };
  void (async () => {
    try {
      if (req.kind === "wasm") {
        await api.deployWasmFile(req.path, req.name, req.opts);
      } else if (req.platform === "android") {
        await api.android.deployStaticTree(req.path, req.name, req.opts, onProgress);
      } else {
        await api.desktop.deployStaticDir(req.path, req.name, req.opts, onProgress);
      }
      await refreshNow();
      if (id !== run) return;
      if (watchers > 0) {
        deployState.update((d) => (d ? { ...d, status: "done" } : d));
      } else {
        deployState.set(null);
        toast(`Deployed ${req.name}`);
      }
    } catch (e) {
      if (id !== run) return;
      deployState.update((d) => (d ? { ...d, status: "failed", error: toUiError(e) } : d));
    }
  })();
  return true;
}

/** Forgets a finished deploy (done or failed). A deploy still sending is never dropped. */
export function dismissDeploy(): void {
  if (!deploying()) deployState.set(null);
}

/** Test helper: back to no deploy at all. */
export function resetDeployForTests(): void {
  run++;
  watchers = 0;
  deployState.set(null);
}
