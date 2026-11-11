import { render, screen } from "@testing-library/svelte";
import { tick } from "svelte";
import { get } from "svelte/store";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { running } from "./routes/fixtures";

const api = vi.hoisted(() => ({
  platform: vi.fn(),
  status: vi.fn(),
  members: vi.fn(),
  services: vi.fn(),
  configGet: vi.fn(),
  logsFollow: vi.fn(),
  desktop: { connection: vi.fn(), locateMeshd: vi.fn() },
  android: { nodeRunning: vi.fn(), powerPolicyGet: vi.fn() },
}));
vi.mock("./lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/api")>()),
  api,
}));

import App from "./App.svelte";
import { resetOtherNodeNoticeForTests } from "./lib/connection";
import { route } from "./lib/router";
import { banner, status } from "./lib/stores";

let app: ReturnType<typeof render> | null = null;

beforeEach(() => {
  vi.clearAllMocks();
  history.replaceState(null, "", "#/");
  dispatchEvent(new HashChangeEvent("hashchange"));
  try {
    localStorage.clear();
  } catch {
    // ignore
  }
  resetOtherNodeNoticeForTests();
  status.set(null);
  banner.set(null);
  api.platform.mockResolvedValue("desktop-windows");
  api.members.mockResolvedValue([]);
  api.services.mockResolvedValue([]);
  api.configGet.mockResolvedValue([]);
  api.desktop.connection.mockResolvedValue({ kind: "system", otherRunning: false });
  api.desktop.locateMeshd.mockResolvedValue("meshd.exe");
  api.android.nodeRunning.mockResolvedValue(false);
  api.android.powerPolicyGet.mockResolvedValue("balanced");
});

afterEach(() => {
  app?.unmount();
  app = null;
});

async function settle() {
  for (let i = 0; i < 8; i++) await tick();
}

it("app_sends_desktop_to_no_daemon_when_unreachable", async () => {
  api.status.mockRejectedValue({
    code: "daemonUnreachable",
    message: "no daemon",
    hint: null,
    reason: null,
  });
  app = render(App);
  await settle();
  expect(get(route).name).toBe("no-daemon");
  expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent(
    "Meshnode isn't running on this computer",
  );
});

it("app_sends_android_to_node_stopped", async () => {
  api.platform.mockResolvedValue("android");
  api.status.mockRejectedValue({
    code: "notRunning",
    message: "stopped",
    hint: null,
    reason: null,
  });
  app = render(App);
  await settle();
  expect(get(route).name).toBe("stopped");
  expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent("Node stopped");
});

it("app_shows_the_second_node_notice_once", async () => {
  api.status.mockResolvedValue(running());
  api.desktop.connection.mockResolvedValue({ kind: "system", otherRunning: true });
  app = render(App);
  await settle();
  expect(screen.getByText(/A second, separate node is running as you/)).toBeInTheDocument();
  app.unmount();

  // A new session (the flag is reset), but storage remembers it was shown.
  resetOtherNodeNoticeForTests();
  app = render(App);
  await settle();
  expect(screen.queryByText(/A second, separate node is running as you/)).toBeNull();
});
