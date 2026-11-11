import { toUiError, type OnEnd, type StreamHandle, type UiError } from "./api";

/**
 * Where a live view (log follow, probe) stands:
 * - `connecting`: asked for, not yet open;
 * - `live`: items are arriving;
 * - `ended`: the stream finished normally;
 * - `disconnected`: the connection was lost (an error event, or the daemon unreachable);
 *   the view offers "Disconnected — Resume";
 * - `busy`: too many live views are open (the daemon allows 4);
 * - `failed`: it could not start for another reason.
 */
export type LiveState =
  | { kind: "idle" }
  | { kind: "connecting" }
  | { kind: "live" }
  | { kind: "ended" }
  | { kind: "disconnected"; error: UiError | null }
  | { kind: "busy"; error: UiError }
  | { kind: "failed"; error: UiError };

export interface Live {
  /** Stops the stream; safe to call before it has opened and more than once. */
  close(): void;
}

/**
 * Opens a stream with `start` and reports its state through `onState`. Closing before the
 * stream has opened cancels it as soon as it does, so an unmounted view never leaks one.
 */
export function openLive(
  start: (onEnd: OnEnd) => Promise<StreamHandle>,
  onState: (s: LiveState) => void,
): Live {
  let closed = false;
  let finished = false;
  let handle: StreamHandle | null = null;
  const cancel = (h: StreamHandle) => void h.cancel().catch(() => {});

  onState({ kind: "connecting" });
  start((error) => {
    if (closed) return;
    finished = true;
    onState(error ? { kind: "disconnected", error } : { kind: "ended" });
  }).then(
    (h) => {
      if (closed) {
        cancel(h);
        return;
      }
      handle = h;
      // An end that raced ahead of the open stays the last word.
      if (!finished) onState({ kind: "live" });
    },
    (e: unknown) => {
      if (closed) return;
      const error = toUiError(e);
      if (error.code === "busy") onState({ kind: "busy", error });
      else if (error.code === "daemonUnreachable") onState({ kind: "disconnected", error });
      else onState({ kind: "failed", error });
    },
  );

  return {
    close() {
      if (closed) return;
      closed = true;
      if (handle && !finished) cancel(handle);
    },
  };
}
