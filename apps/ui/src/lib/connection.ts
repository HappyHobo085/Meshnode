import type { ConnectionDto } from "./api";

const KEY = "meshnode.otherNodeNoticeShown";

/**
 * Whether to show the one-time notice that a second, separate node runs as the user (both
 * the service and a per-user daemon answer; the app uses the service). Showing it once
 * marks it shown. Without storage it shows once per session.
 */
let shownThisSession = false;
export function takeOtherNodeNotice(c: ConnectionDto | null): boolean {
  if (!c?.otherRunning || shownThisSession) return false;
  shownThisSession = true;
  try {
    if (localStorage.getItem(KEY) === "1") return false;
    localStorage.setItem(KEY, "1");
  } catch {
    // Storage unavailable: once per session is the best we can do.
  }
  return true;
}

/** For tests: forget the session flag. */
export function resetOtherNodeNoticeForTests(): void {
  shownThisSession = false;
}
