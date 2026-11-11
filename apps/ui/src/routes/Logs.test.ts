import { fireEvent, render, screen } from "@testing-library/svelte";
import { flushSync, tick } from "svelte";
import { beforeEach, expect, it, vi } from "vitest";
import type { OnEnd } from "../lib/api";

const api = vi.hoisted(() => ({
  platform: vi.fn(),
  logsFollow: vi.fn(),
}));
vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  api,
}));

import Logs from "./Logs.svelte";

let push: (line: string) => void;
let end: OnEnd;
let cancel: ReturnType<typeof vi.fn>;

beforeEach(() => {
  vi.clearAllMocks();
  cancel = vi.fn().mockResolvedValue(undefined);
  api.platform.mockResolvedValue("desktop-windows");
  api.logsFollow.mockImplementation(
    async (_lines: number, onLine: (l: string) => void, onEnd: OnEnd) => {
      push = onLine;
      end = onEnd;
      return { id: 7, cancel };
    },
  );
});

const line = (level: string, msg: string) =>
  `2026-11-11T10:00:00.000Z ${level.padStart(5)} mesh: ${msg}`;

async function mount() {
  const r = render(Logs);
  await tick();
  await tick();
  return r;
}

it("logs_view_keeps_10000_and_renders_fast", async () => {
  await mount();
  expect(api.logsFollow).toHaveBeenCalledTimes(1);
  let n = 0;
  for (let batch = 0; batch < 24; batch++) {
    const t0 = performance.now();
    for (let i = 0; i < 500; i++) push(line("INFO", `line ${n++}`));
    flushSync();
    const ms = performance.now() - t0;
    expect(ms, `batch ${batch}`).toBeLessThan(50);
  }
  expect(n).toBe(12_000);
  expect(screen.getByText(/^10\D?000 lines$/)).toBeInTheDocument();
  // Following the tail: the newest line is drawn, the dropped ones are gone, and only the
  // rows in view exist in the DOM.
  expect(screen.getByText(/line 11999$/)).toBeInTheDocument();
  expect(screen.queryByText(/line 1999$/)).toBeNull();
  const rows = document.querySelectorAll(".log-row");
  expect(rows.length).toBeGreaterThan(0);
  expect(rows.length).toBeLessThan(120);
});

it("logs_view_cancels_on_unmount", async () => {
  const { unmount } = await mount();
  unmount();
  expect(cancel).toHaveBeenCalledTimes(1);
});

it("logs_view_cancels_a_stream_that_opens_after_unmount", async () => {
  let resolve!: (h: { id: number; cancel: typeof cancel }) => void;
  api.logsFollow.mockImplementation(() => new Promise((r) => (resolve = r)));
  const { unmount } = render(Logs);
  await tick();
  unmount();
  resolve({ id: 9, cancel });
  await tick();
  await tick();
  expect(cancel).toHaveBeenCalledTimes(1);
});

it("logs_pause_buffers_then_resumes", async () => {
  await mount();
  push(line("INFO", "first"));
  flushSync();
  expect(screen.getByText(/first$/)).toBeInTheDocument();
  await fireEvent.click(screen.getByRole("button", { name: "Pause" }));
  push(line("INFO", "while paused"));
  push(line("WARN", "also paused"));
  flushSync();
  expect(screen.queryByText(/while paused$/)).toBeNull();
  expect(screen.getByText("2 new lines")).toBeInTheDocument();
  await fireEvent.click(screen.getByRole("button", { name: "Resume" }));
  expect(screen.getByText(/while paused$/)).toBeInTheDocument();
  expect(screen.getByText(/also paused$/)).toBeInTheDocument();
  expect(screen.queryByText("2 new lines")).toBeNull();
});

it("logs_level_filter", async () => {
  await mount();
  push(line("INFO", "hello"));
  push(line("WARN", "careful"));
  push("   continuation of the warning");
  push(line("ERROR", "broken"));
  push(line("DEBUG", "detail"));
  flushSync();
  for (const l of ["Trace", "Debug", "Info", "Warn", "Error"]) {
    expect(screen.getByRole("button", { name: l })).toHaveAttribute("aria-pressed", "true");
  }
  await fireEvent.click(screen.getByRole("button", { name: "Info" }));
  await fireEvent.click(screen.getByRole("button", { name: "Debug" }));
  expect(screen.getByRole("button", { name: "Info" })).toHaveAttribute("aria-pressed", "false");
  expect(screen.queryByText(/hello$/)).toBeNull();
  expect(screen.queryByText(/detail$/)).toBeNull();
  expect(screen.getByText(/careful$/)).toBeInTheDocument();
  // A continuation line keeps the level of the line it continues.
  expect(screen.getByText(/continuation of the warning$/)).toBeInTheDocument();
  expect(screen.getByText(/broken$/)).toBeInTheDocument();
  await fireEvent.click(screen.getByRole("button", { name: "Warn" }));
  expect(screen.queryByText(/continuation of the warning$/)).toBeNull();
});

it("logs_search_filters_and_marks_the_match", async () => {
  await mount();
  push(line("INFO", "peer Pixel joined"));
  push(line("INFO", "gateway listening"));
  flushSync();
  await fireEvent.input(screen.getByRole("searchbox", { name: "Search logs" }), {
    target: { value: "pixel" },
  });
  expect(screen.queryByText(/gateway listening/)).toBeNull();
  const mark = document.querySelector(".log-row mark");
  expect(mark).toHaveTextContent("Pixel");
  expect(screen.getByText("1 of 2 lines")).toBeInTheDocument();
});

it("logs_disconnected_offers_resume", async () => {
  await mount();
  push(line("INFO", "before"));
  end({ code: "daemonUnreachable", message: "gone", hint: null, reason: null });
  flushSync();
  expect(screen.getByRole("status")).toHaveTextContent("Disconnected");
  await fireEvent.click(screen.getByRole("button", { name: "Resume" }));
  await tick();
  expect(api.logsFollow).toHaveBeenCalledTimes(2);
  await tick();
  expect(screen.queryAllByText("Disconnected")).toHaveLength(0);
});

it("logs_too_many_live_views_shows_inline", async () => {
  api.logsFollow.mockRejectedValue({
    code: "busy",
    message: "Too many live views open",
    hint: null,
    reason: null,
  });
  await mount();
  await tick();
  expect(screen.getByRole("alert")).toHaveTextContent("Too many live views open");
  expect(screen.getByRole("button", { name: "Try again" })).toBeInTheDocument();
});
