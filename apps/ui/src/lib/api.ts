// The one door to Tauri. Every other module talks to the backend through `api`, so tests
// mock this file and nothing else imports `@tauri-apps/*` (an ESLint rule enforces it).
//
// Argument names: the Rust commands take snake_case parameters, but Tauri 2 expects the
// keys of `invoke`'s argument object in camelCase (`ttl_s` -> `ttlS`, `id_hex` -> `idHex`,
// `stream_id` -> `streamId`, `on_event` -> `onEvent`). The command names stay snake_case.
import { Channel, invoke } from "@tauri-apps/api/core";
import { Format, scan } from "@tauri-apps/plugin-barcode-scanner";
import type { ConfigEntry } from "./generated/ConfigEntry";
import type { ConfigSetDto } from "./generated/ConfigSetDto";
import type { ConnectionDto } from "./generated/ConnectionDto";
import type { DeployOptsDto } from "./generated/DeployOptsDto";
import type { InviteDto } from "./generated/InviteDto";
import type { LimitsDto } from "./generated/LimitsDto";
import type { MemberDto } from "./generated/MemberDto";
import type { PlacementDto } from "./generated/PlacementDto";
import type { Platform } from "./generated/Platform";
import type { ProbeSampleDto } from "./generated/ProbeSampleDto";
import type { RoleDto } from "./generated/RoleDto";
import type { ServiceDto } from "./generated/ServiceDto";
import type { StatusDto } from "./generated/StatusDto";
import type { StreamEvent } from "./generated/StreamEvent";
import type { UiError } from "./generated/UiError";
import type { UiErrorCode } from "./generated/UiErrorCode";

export type {
  ConfigEntry,
  ConfigSetDto,
  ConnectionDto,
  DeployOptsDto,
  InviteDto,
  LimitsDto,
  MemberDto,
  PlacementDto,
  Platform,
  ProbeSampleDto,
  RoleDto,
  ServiceDto,
  StatusDto,
  UiError,
  UiErrorCode,
};

/** Android's power policy (ruling R6: exactly these values). */
export type PowerPolicy = "balanced" | "always-on" | "saver";

/** A live view (log follow, probe) that can be stopped. */
export interface StreamHandle {
  id: number;
  cancel(): Promise<void>;
}

/** Called once when a stream finishes: `null` for a normal end, or the error that ended it. */
export type OnEnd = (error: UiError | null) => void;

export interface ProbeArgs {
  /** Hex prefix of the node id to probe. */
  nodePrefix: string;
  count: number;
  intervalMs: number;
  timeoutMs: number;
}

const CODES: ReadonlySet<string> = new Set<UiErrorCode>([
  "notRunning",
  "wrongState",
  "invalidArgument",
  "notFound",
  "joinRefused",
  "lastHost",
  "busy",
  "internal",
  "daemonUnreachable",
  "daemonStarting",
  "cancelled",
  "pickerCancelled",
  "io",
]);

export function isUiError(e: unknown): e is UiError {
  return (
    typeof e === "object" &&
    e !== null &&
    typeof (e as { code?: unknown }).code === "string" &&
    CODES.has((e as { code: string }).code) &&
    typeof (e as { message?: unknown }).message === "string"
  );
}

/** Anything thrown by Tauri (a `UiError`, an ACL refusal string, an Error) as a `UiError`. */
export function toUiError(e: unknown): UiError {
  if (isUiError(e)) {
    return { code: e.code, message: e.message, hint: e.hint ?? null, reason: e.reason ?? null };
  }
  const message = e instanceof Error ? e.message : typeof e === "string" ? e : String(e);
  return { code: "internal", message, hint: null, reason: null };
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    throw toUiError(e);
  }
}

/** A picker that rejects with `pickerCancelled` when nothing is chosen, as `string | null`. */
async function picked(p: Promise<string | null>): Promise<string | null> {
  try {
    return await p;
  } catch (e) {
    if (isUiError(e) && e.code === "pickerCancelled") return null;
    throw e;
  }
}

async function stream<T>(
  cmd: string,
  args: Record<string, unknown>,
  onItem: (item: T) => void,
  onEnd?: OnEnd,
): Promise<StreamHandle> {
  let ended = false;
  const finish = (error: UiError | null) => {
    if (ended) return;
    ended = true;
    onEnd?.(error);
  };
  const channel = new Channel<StreamEvent<T>>((event) => {
    if (ended) return;
    switch (event.kind) {
      case "item":
        onItem(event.value);
        break;
      case "end":
        finish(null);
        break;
      case "error":
        finish(toUiError(event.error));
        break;
    }
  });
  const id = await call<number>(cmd, { ...args, onEvent: channel });
  return {
    id,
    cancel: async () => {
      ended = true;
      await call<void>("cancel_stream", { streamId: id });
    },
  };
}

function progressChannel(onProgress?: (bytes: number) => void): Channel<number> {
  return new Channel<number>((n) => onProgress?.(n));
}

export const api = {
  status: () => call<StatusDto>("status"),
  members: () => call<MemberDto[]>("members"),
  services: () => call<ServiceDto[]>("services"),
  init: (name: string) => call<StatusDto>("init", { name }),
  join: (ticket: string, name: string) => call<StatusDto>("join", { ticket, name }),
  invite: (role: RoleDto, ttlS: number) => call<InviteDto>("invite", { role, ttlS }),
  remove: (idHex: string) => call<void>("remove", { idHex }),
  /** Change this device's name and/or tags; an omitted field is left as it is. */
  setInfo: (info: { name?: string; tags?: string[] }) =>
    call<void>("set_info", { name: info.name ?? null, tags: info.tags ?? null }),
  setTagsOf: (idHex: string, tags: string[]) => call<void>("set_tags_of", { idHex, tags }),
  deployWasmFile: (path: string, name: string, opts: DeployOptsDto) =>
    call<ServiceDto>("deploy_wasm_file", { path, name, opts }),
  undeploy: (name: string) => call<void>("undeploy", { name }),
  purge: (name: string) => call<void>("purge", { name }),
  setPlacement: (name: string, placement: PlacementDto) =>
    call<void>("set_placement", { name, placement }),
  setOutbound: (name: string, outbound: string[]) => call<void>("set_outbound", { name, outbound }),
  setLimits: (name: string, limits: LimitsDto) => call<void>("set_limits", { name, limits }),
  configGet: () => call<ConfigEntry[]>("config_get"),
  configSet: (key: string, value: string) => call<ConfigSetDto>("config_set", { key, value }),
  resetIdentity: () => call<StatusDto>("reset_identity"),
  platform: () => call<Platform>("platform"),

  /** Follow the daemon log: the last `lines` lines, then new ones as they arrive. */
  logsFollow: (lines: number, onLine: (line: string) => void, onEnd?: OnEnd) =>
    stream<string>("logs_follow", { lines }, onLine, onEnd),
  /** Latency probe of one node: `count` samples, one every `intervalMs`. */
  probe: (args: ProbeArgs, onSample: (s: ProbeSampleDto) => void, onEnd?: OnEnd) =>
    stream<ProbeSampleDto>("probe", { ...args }, onSample, onEnd),
  cancelStream: (streamId: number) => call<void>("cancel_stream", { streamId }),

  /** Desktop-only commands. */
  desktop: {
    /** Deploy a site folder; `onProgress` receives the bytes sent so far. */
    deployStaticDir: (
      path: string,
      name: string,
      opts: DeployOptsDto,
      onProgress?: (bytes: number) => void,
    ) =>
      call<ServiceDto>("deploy_static_dir", {
        path,
        name,
        opts,
        progress: progressChannel(onProgress),
      }),
    connection: () => call<ConnectionDto>("connection"),
    locateMeshd: () => call<string | null>("locate_meshd"),
    setMeshdPath: (path: string) => call<void>("set_meshd_path", { path }),
    daemonRunUser: () => call<ConnectionDto>("daemon_run_user"),
    daemonInstall: () => call<ConnectionDto>("daemon_install"),
    /** A folder from the system dialog, or `null` when cancelled. */
    pickFolder: () => picked(call<string>("pick_folder")),
    /** A file with extension `ext` (e.g. "wasm") from the system dialog, or `null`. */
    pickFile: (ext: string) => picked(call<string>("pick_file", { ext })),
    openUrl: (url: string) => call<void>("open_url", { url }),
    /** This machine's host name, offered as the device name on First run. */
    defaultDeviceName: () => call<string>("default_device_name"),
  },

  /** Android-only commands (the Rust side lands with the Android shell). */
  android: {
    nodeRunning: () => call<boolean>("node_running"),
    serviceStart: () => call<void>("service_start"),
    serviceStop: () => call<void>("service_stop"),
    powerPolicyGet: () => call<PowerPolicy>("power_policy_get"),
    powerPolicySet: (policy: PowerPolicy) => call<void>("power_policy_set", { policy }),
    /** A folder (SAF tree URI), or `null` when cancelled. */
    pickTree: () => picked(call<string | null>("pick_tree")),
    /** A `.wasm` file copied to the app cache (its path), or `null` (ruling R5). */
    pickWasm: () => picked(call<string | null>("pick_wasm")),
    deployStaticTree: (
      treeUri: string,
      name: string,
      opts: DeployOptsDto,
      onProgress?: (bytes: number) => void,
    ) =>
      call<ServiceDto>("deploy_static_tree", {
        treeUri,
        name,
        opts,
        progress: progressChannel(onProgress),
      }),
    /** Scan one QR code with the camera; resolves with its text. */
    scan: async (): Promise<string> => {
      try {
        const result = await scan({ formats: [Format.QRCode] });
        return result.content;
      } catch (e) {
        throw toUiError(e);
      }
    },
  },
};

export type Api = typeof api;
