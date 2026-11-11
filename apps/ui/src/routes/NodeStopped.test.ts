import { fireEvent, render, screen } from "@testing-library/svelte";
import { tick } from "svelte";
import { get } from "svelte/store";
import { beforeEach, expect, it, vi } from "vitest";
import { running } from "./fixtures";

const api = vi.hoisted(() => ({
  platform: vi.fn(),
  status: vi.fn(),
  members: vi.fn(),
  services: vi.fn(),
  android: { nodeRunning: vi.fn(), serviceStart: vi.fn() },
}));
vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  api,
}));

import { route } from "../lib/router";
import { banner, status } from "../lib/stores";
import NodeStopped from "./NodeStopped.svelte";

const STOPPED = {
  code: "notRunning" as const,
  message: "stopped",
  hint: "Start the node.",
  reason: null,
};

beforeEach(() => {
  vi.clearAllMocks();
  history.replaceState(null, "", "#/stopped");
  dispatchEvent(new HashChangeEvent("hashchange"));
  api.platform.mockResolvedValue("android");
  api.status.mockRejectedValue(STOPPED);
  api.members.mockResolvedValue([]);
  api.services.mockResolvedValue([]);
  api.android.nodeRunning.mockResolvedValue(false);
  status.set(null);
  banner.set({ kind: "stopped", error: STOPPED });
});

it("node_stopped_start_calls_service_start", async () => {
  api.android.serviceStart.mockImplementation(async () => {
    api.status.mockResolvedValue(running());
  });
  render(NodeStopped);
  await tick();
  expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent("Node stopped");
  await fireEvent.click(screen.getByRole("button", { name: "Start" }));
  expect(api.android.serviceStart).toHaveBeenCalledTimes(1);
  for (let i = 0; i < 6; i++) await tick();
  expect(get(route).name).toBe("home");
});

it("node_stopped_start_failure_shows_inline", async () => {
  api.android.serviceStart.mockRejectedValue({
    code: "internal",
    message: "service refused to start",
    hint: null,
    reason: null,
  });
  render(NodeStopped);
  await tick();
  await fireEvent.click(screen.getByRole("button", { name: "Start" }));
  await tick();
  await tick();
  expect(screen.getByRole("alert")).toHaveTextContent("service refused to start");
  expect(get(route).name).toBe("stopped");
});
