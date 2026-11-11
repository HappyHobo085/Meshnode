import { beforeEach, describe, expect, it, vi } from "vitest";

const core = vi.hoisted(() => {
  class Channel<T> {
    onmessage: (m: T) => void;
    constructor(onmessage?: (m: T) => void) {
      this.onmessage = onmessage ?? (() => {});
    }
  }
  return { invoke: vi.fn(), Channel };
});
vi.mock("@tauri-apps/api/core", () => core);
vi.mock("@tauri-apps/plugin-barcode-scanner", () => ({
  scan: vi.fn().mockResolvedValue({ content: "mesh1ticket", format: "QR_CODE" }),
  Format: { QRCode: "QR_CODE" },
}));

import { api } from "./api";

// A block body: a function returned from beforeEach would run as teardown.
beforeEach(() => {
  core.invoke.mockReset();
});

describe("api", () => {
  it("passes_camel_case_args_to_tauri", async () => {
    core.invoke.mockResolvedValue(undefined);
    await api.invite("client", 900);
    expect(core.invoke).toHaveBeenCalledWith("invite", { role: "client", ttlS: 900 });
    await api.remove("ab12");
    expect(core.invoke).toHaveBeenCalledWith("remove", { idHex: "ab12" });
    await api.setTagsOf("ab12", ["home"]);
    expect(core.invoke).toHaveBeenCalledWith("set_tags_of", { idHex: "ab12", tags: ["home"] });
    await api.setInfo({ name: "desk" });
    expect(core.invoke).toHaveBeenCalledWith("set_info", { name: "desk", tags: null });
  });

  it("rejects_with_ui_error", async () => {
    core.invoke.mockRejectedValue({
      code: "notRunning",
      message: "node is stopped",
      hint: "start it",
      reason: null,
    });
    await expect(api.status()).rejects.toEqual({
      code: "notRunning",
      message: "node is stopped",
      hint: "start it",
      reason: null,
    });
  });

  it("wraps_plain_string_errors", async () => {
    core.invoke.mockRejectedValue("command status not allowed");
    await expect(api.status()).rejects.toEqual({
      code: "internal",
      message: "command status not allowed",
      hint: null,
      reason: null,
    });
  });

  it("streams_items_and_cancels", async () => {
    core.invoke.mockResolvedValueOnce(7);
    const lines: string[] = [];
    let ended: unknown = "not yet";
    const handle = await api.logsFollow(
      100,
      (l) => lines.push(l),
      (e) => (ended = e),
    );
    expect(handle.id).toBe(7);
    const [cmd, args] = core.invoke.mock.calls[0] as [string, Record<string, unknown>];
    expect(cmd).toBe("logs_follow");
    expect(args.lines).toBe(100);
    const channel = args.onEvent as { onmessage: (m: unknown) => void };
    channel.onmessage({ kind: "item", value: "hello" });
    channel.onmessage({ kind: "end" });
    expect(lines).toEqual(["hello"]);
    expect(ended).toBeNull();

    core.invoke.mockResolvedValueOnce(undefined);
    await handle.cancel();
    expect(core.invoke).toHaveBeenLastCalledWith("cancel_stream", { streamId: 7 });
  });

  it("stream_error_reaches_on_end", async () => {
    core.invoke.mockResolvedValueOnce(3);
    let ended: unknown;
    await api.probe(
      { nodePrefix: "ab", count: 5, intervalMs: 500, timeoutMs: 2000 },
      () => {},
      (e) => (ended = e),
    );
    const args = core.invoke.mock.calls[0]?.[1] as Record<string, unknown>;
    expect(args).toMatchObject({ nodePrefix: "ab", count: 5, intervalMs: 500, timeoutMs: 2000 });
    const err = { code: "notFound", message: "no such node", hint: null, reason: null };
    (args.onEvent as { onmessage: (m: unknown) => void }).onmessage({ kind: "error", error: err });
    expect(ended).toEqual(err);
  });

  it("android_scan_returns_the_content", async () => {
    await expect(api.android.scan()).resolves.toBe("mesh1ticket");
  });
});
