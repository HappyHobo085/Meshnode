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
  desktop: {
    connection: vi.fn(),
    daemonRunUser: vi.fn(),
    daemonInstall: vi.fn(),
    locateMeshd: vi.fn(),
    setMeshdPath: vi.fn(),
    pickFile: vi.fn(),
  },
}));
vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  api,
}));

import { route } from "../lib/router";
import { banner, status } from "../lib/stores";
import NoDaemon from "./NoDaemon.svelte";

const UNREACHABLE = {
  code: "daemonUnreachable" as const,
  message: "no daemon",
  hint: "Start Meshnode, or install it as a service.",
  reason: null,
};

beforeEach(() => {
  vi.clearAllMocks();
  history.replaceState(null, "", "#/no-daemon");
  dispatchEvent(new HashChangeEvent("hashchange"));
  api.platform.mockResolvedValue("desktop-windows");
  api.status.mockRejectedValue(UNREACHABLE);
  api.members.mockResolvedValue([]);
  api.services.mockResolvedValue([]);
  api.desktop.connection.mockResolvedValue({ kind: "none", otherRunning: false });
  api.desktop.locateMeshd.mockResolvedValue("C:/Program Files/Meshnode/meshd.exe");
  api.desktop.setMeshdPath.mockResolvedValue(undefined);
  status.set(null);
  banner.set({ kind: "unreachable", error: UNREACHABLE });
});

async function mount() {
  const r = render(NoDaemon);
  for (let i = 0; i < 3; i++) await tick();
  return r;
}

it("no_daemon_actions", async () => {
  // Run without installing: "Starting…", then Home once a poll succeeds.
  let started!: () => void;
  api.desktop.daemonRunUser.mockImplementation(
    () => new Promise((r) => (started = () => r({ kind: "user", otherRunning: false }))),
  );
  await mount();
  expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent(
    "Meshnode isn't running on this computer",
  );
  await fireEvent.click(screen.getByRole("button", { name: "Run without installing" }));
  expect(api.desktop.daemonRunUser).toHaveBeenCalledTimes(1);
  expect(screen.getByRole("status")).toHaveTextContent("Starting…");
  expect(get(route).name).toBe("no-daemon");

  api.status.mockResolvedValue(running());
  started();
  for (let i = 0; i < 6; i++) await tick();
  expect(get(route).name).toBe("home");
});

it("no_daemon_install_cancelled", async () => {
  api.desktop.daemonInstall.mockRejectedValue({
    code: "cancelled",
    message: "Installation cancelled",
    hint: null,
    reason: null,
  });
  await mount();
  await fireEvent.click(screen.getByRole("button", { name: "Install as a service" }));
  await tick();
  await tick();
  expect(api.desktop.daemonInstall).toHaveBeenCalledTimes(1);
  expect(screen.getByRole("alert")).toHaveTextContent("Installation cancelled");
  expect(get(route).name).toBe("no-daemon");
});

it("no_daemon_run_that_never_answers_points_at_the_log", async () => {
  api.desktop.daemonRunUser.mockRejectedValue({
    code: "daemonStarting",
    message: "Meshnode has not answered yet",
    hint: null,
    reason: null,
  });
  await mount();
  await fireEvent.click(screen.getByRole("button", { name: "Run without installing" }));
  await tick();
  await tick();
  const alert = screen.getByRole("alert");
  expect(alert).toHaveTextContent("Meshnode didn't start");
  expect(alert).toHaveTextContent(/Meshnode[\\/]meshd\.log/);
});

it("no_daemon_without_meshd_offers_locate", async () => {
  api.desktop.locateMeshd.mockResolvedValueOnce(null);
  api.desktop.pickFile.mockResolvedValue("D:/tools/meshd.exe");
  await mount();
  expect(screen.getByText(/meshd wasn't found/)).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Run without installing" })).toBeDisabled();
  await fireEvent.click(screen.getByRole("button", { name: "Locate meshd…" }));
  await tick();
  await tick();
  expect(api.desktop.pickFile).toHaveBeenCalledWith("exe");
  expect(api.desktop.setMeshdPath).toHaveBeenCalledWith("D:/tools/meshd.exe");
  await tick();
  await tick();
  expect(screen.getByRole("button", { name: "Run without installing" })).toBeEnabled();
});
