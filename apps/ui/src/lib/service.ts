// Service helpers shared by the Services list, the Service detail sheet and Deploy.
import type { IconName } from "./design/Icon.svelte";
import type { LimitsDto, MemberDto, PlacementDto, ServiceDto } from "./api";

/** The daemon's defaults for a new service (mesh-proto `Limits::default`). */
export const DEFAULT_LIMITS: LimitsDto = {
  memoryMb: 64,
  requestTimeoutMs: 10_000,
  maxBodyBytes: 8 * 1024 * 1024,
};

/** The daemon's bounds (mesh-proto `service.rs`). */
export const LIMIT_BOUNDS = {
  memoryMb: [1, 1024],
  timeoutS: [0.1, 300],
  bodyMiB: [0, 64],
} as const;
export const MAX_OUTBOUND = 64;
export const MAX_PLACEMENT = 64;
export const MAX_TAG = 32;
export const MIB = 1024 * 1024;

/** `[a-z0-9-]{1,32}`, as the daemon checks it. */
export function validServiceName(name: string): boolean {
  return /^[a-z0-9-]{1,32}$/.test(name);
}
export const NAME_RULE = "Use 1 to 32 lowercase letters, digits and dashes";

/**
 * A service name offered from a picked path: its last segment (a file loses its extension),
 * lowercased, with everything outside the daemon's alphabet turned into single dashes.
 * Android tree URIs are percent-encoded (`…/tree/primary%3ASites%2Fblog`), so decode first.
 */
export function nameFromPath(path: string): string {
  let decoded = path;
  try {
    decoded = decodeURIComponent(path);
  } catch {
    // A path with a stray % stays as it is.
  }
  const last =
    decoded
      .split(/[/\\:]/)
      .filter(Boolean)
      .pop() ?? "";
  const base = last.replace(/\.wasm$/i, "");
  return base
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 32)
    .replace(/-+$/, "");
}

/** The last segment of a path or tree URI, for showing what was picked. */
export function displayPath(path: string): string {
  let decoded = path;
  try {
    decoded = decodeURIComponent(path);
  } catch {
    // Shown as it is.
  }
  return decoded.replace(/^content:\/\/[^/]+\/tree\//, "");
}

/** A `host:port` outbound entry the daemon accepts (non-empty host, a u16 port). */
export function validOutbound(entry: string): boolean {
  const at = entry.lastIndexOf(":");
  if (at <= 0) return false;
  const host = entry.slice(0, at);
  const port = entry.slice(at + 1);
  return !/\s/.test(host) && /^\d{1,5}$/.test(port) && Number(port) <= 65_535;
}

/** A host's accent hue from its id, as Rust derives it: first two bytes, mod 360. */
export function hueOf(id: string): number {
  const n = parseInt(id.slice(0, 4), 16);
  return Number.isNaN(n) ? 172 : n % 360;
}

export interface HostView {
  id: string;
  name: string;
  hue: number;
}

/** The hosts running `s`, named and coloured from `members` (an unknown id by its prefix). */
export function hostsOf(s: ServiceDto, members: MemberDto[]): HostView[] {
  const byId = new Map(members.map((m) => [m.id, m]));
  return s.placedHosts.map((id) => {
    const m = byId.get(id);
    return m
      ? { id, name: m.name, hue: m.accentHue }
      : { id, name: id.slice(0, 6), hue: hueOf(id) };
  });
}

/** "Desk", "Desk and Pixel", "Desk, Pixel and Server". */
export function listNames(names: string[]): string {
  if (names.length <= 1) return names[0] ?? "";
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

export function kindOf(kind: ServiceDto["kind"]): { label: string; icon: IconName } {
  return kind === "wasm"
    ? { label: "WebAssembly", icon: "file" }
    : { label: "Static site", icon: "folder" };
}

export type Tone = "ok" | "warn" | "danger" | "quiet";

/** How the service is doing on this device, in words (with a tone for colour). */
export function localState(s: ServiceDto): { text: string; tone: Tone; detail?: string } {
  switch (s.local.kind) {
    case "ready":
      return s.local.version >= s.version
        ? { text: "Running here", tone: "ok" }
        : { text: `Running v${s.local.version} here`, tone: "warn" };
    case "fetching":
      return { text: "Downloading here", tone: "warn" };
    case "failed":
      return { text: "Failed here", tone: "danger", detail: s.local.error };
    case "notPlaced":
      return { text: "Not on this device", tone: "quiet" };
  }
}

/** Members that can run services: effective hosts. */
export function hostMembers(members: MemberDto[]): MemberDto[] {
  return members.filter((m) => m.role === "host" && m.effective);
}

/** Which of `hosts` a placement selects (the daemon's rule, for a live preview). */
export function selectedBy(p: PlacementDto, hosts: MemberDto[]): MemberDto[] {
  switch (p.kind) {
    case "allHosts":
      return hosts;
    case "tags":
      return hosts.filter((h) => h.tags.some((t) => p.tags.includes(t)));
    case "nodes":
      return hosts.filter((h) => p.ids.includes(h.id));
  }
}

/** A placement that can be sent: tags or nodes need at least one entry. */
export function placementReady(p: PlacementDto): boolean {
  if (p.kind === "tags") return p.tags.length > 0 && p.tags.length <= MAX_PLACEMENT;
  if (p.kind === "nodes") return p.ids.length > 0 && p.ids.length <= MAX_PLACEMENT;
  return true;
}

export function samePlacement(a: PlacementDto, b: PlacementDto): boolean {
  if (a.kind !== b.kind) return false;
  const set = (p: PlacementDto) =>
    p.kind === "tags" ? [...p.tags].sort() : p.kind === "nodes" ? [...p.ids].sort() : [];
  const [x, y] = [set(a), set(b)];
  return x.length === y.length && x.every((v, i) => v === y[i]);
}

/** A limit value for an input: no trailing zeros ("10", "2.5", "0.75"). */
export function plain(n: number): string {
  return String(Math.round(n * 100) / 100);
}
