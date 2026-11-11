import { writable } from "svelte/store";
import { api } from "./api";
import type { MemberDto, ServiceDto, StatusDto, UiError } from "./api";

/** The daemon's status, `null` until the first answer. */
export const status = writable<StatusDto | null>(null);
/** Mesh members (empty until the node runs). */
export const members = writable<MemberDto[]>([]);
/** Deployed services (empty until the node runs). */
export const services = writable<ServiceDto[]>([]);

export type BannerKind = "unreachable" | "stopped" | "starting";
export interface BannerState {
  kind: BannerKind;
  error?: UiError;
}
/** The top banner: the daemon can't be reached, the node is stopped, or it is starting. */
export const banner = writable<BannerState | null>(null);

const BANNER_FOR: Partial<Record<UiError["code"], BannerKind>> = {
  daemonUnreachable: "unreachable",
  notRunning: "stopped",
  daemonStarting: "starting",
};

/** Applies a poll failure: the daemon-level codes raise the banner, others are left to screens. */
function fail(e: unknown): void {
  const error = e as UiError;
  const kind = error && typeof error === "object" ? BANNER_FOR[error.code] : undefined;
  if (kind) banner.set({ kind, error });
}

async function pollOnce(): Promise<void> {
  try {
    const s = await api.status();
    status.set(s);
    if (s.state === "running") {
      const [m, sv] = await Promise.all([api.members(), api.services()]);
      members.set(m);
      services.set(sv);
    } else {
      members.set([]);
      services.set([]);
    }
    banner.set(null);
  } catch (e) {
    fail(e);
  }
}

let inFlight: Promise<void> | null = null;
let again: Promise<void> | null = null;

/** One poll, unless one is already running (then that one's promise). */
function poll(): Promise<void> {
  if (!inFlight) {
    inFlight = pollOnce().finally(() => {
      inFlight = null;
    });
  }
  return inFlight;
}

/**
 * Refresh now, e.g. right after a mutating command. If a poll is already running it may
 * have read the state from before the mutation, so a fresh one follows it.
 */
export function refreshNow(): Promise<void> {
  if (!inFlight) return poll();
  again ??= inFlight.then(() => {
    again = null;
    return poll();
  });
  return again;
}

type VisibilityDoc = Pick<Document, "visibilityState" | "addEventListener" | "removeEventListener">;

/**
 * Polls status, members and services every `intervalMs` while the page is visible (this
 * also pauses the Android app in the background). A tick while a poll is pending is
 * skipped, so requests never overlap. Returns `stop()`.
 */
export function startPoller({
  intervalMs = 2000,
  doc = document,
}: { intervalMs?: number; doc?: VisibilityDoc } = {}): () => void {
  const visible = () => doc.visibilityState === "visible";
  const tick = () => {
    if (visible() && !inFlight) void poll();
  };
  const onVisibility = () => tick();

  const timer = setInterval(tick, intervalMs);
  doc.addEventListener("visibilitychange", onVisibility);
  tick();
  return () => {
    clearInterval(timer);
    doc.removeEventListener("visibilitychange", onVisibility);
  };
}

export interface ToastItem {
  id: number;
  message: string;
}
/** Success toasts (errors are shown inline, near the control that failed). */
export const toasts = writable<ToastItem[]>([]);
let toastSeq = 0;

export function toast(message: string, ms = 3200): void {
  const id = ++toastSeq;
  toasts.update((t) => [...t, { id, message }]);
  setTimeout(() => dismissToast(id), ms);
}

export function dismissToast(id: number): void {
  toasts.update((t) => t.filter((x) => x.id !== id));
}
