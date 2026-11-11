import { fireEvent, render, screen, within } from "@testing-library/svelte";
import { flushSync, tick } from "svelte";
import { get } from "svelte/store";
import { beforeEach, expect, it, vi } from "vitest";
import type { OnEnd, ProbeSampleDto } from "../lib/api";
import { member, running } from "./fixtures";

const api = vi.hoisted(() => ({
  platform: vi.fn(),
  status: vi.fn(),
  members: vi.fn(),
  services: vi.fn(),
  setInfo: vi.fn(),
  configGet: vi.fn(),
  configSet: vi.fn(),
  resetIdentity: vi.fn(),
  probe: vi.fn(),
  desktop: { connection: vi.fn() },
  android: { powerPolicyGet: vi.fn(), powerPolicySet: vi.fn() },
}));
vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  api,
}));

import { members, status, toasts } from "../lib/stores";
import { theme } from "../lib/theme";
import Settings from "./Settings.svelte";

const DESK = member({ id: "aa11", name: "Desk", isSelf: true, tags: ["desktop"], accentHue: 30 });
const PIXEL = member({ id: "bb22", name: "Pixel", platform: "android", accentHue: 250 });

let sample: (s: ProbeSampleDto) => void;
let probeEnd: OnEnd;
let cancel: ReturnType<typeof vi.fn>;

beforeEach(() => {
  vi.clearAllMocks();
  cancel = vi.fn().mockResolvedValue(undefined);
  api.platform.mockResolvedValue("desktop-windows");
  api.status.mockResolvedValue(running({ tags: ["desktop"] }));
  api.members.mockResolvedValue([DESK, PIXEL]);
  api.services.mockResolvedValue([]);
  api.setInfo.mockResolvedValue(undefined);
  api.configGet.mockResolvedValue([
    { key: "name", value: "Desk" },
    { key: "tags", value: "desktop" },
    { key: "relay", value: "default" },
    { key: "discovery", value: "true" },
    { key: "log_level", value: "info" },
    { key: "gateway.listen", value: "127.0.0.1:7800" },
    { key: "gateway.ports", value: "7801=sync" },
    { key: "services.max_memory_mb", value: "" },
    { key: "admin.windows_user_sid", value: "" },
  ]);
  api.configSet.mockResolvedValue({ restartRequired: true });
  api.resetIdentity.mockResolvedValue(running({ state: "uninitialised" }));
  api.desktop.connection.mockResolvedValue({ kind: "system", otherRunning: false });
  api.android.powerPolicyGet.mockResolvedValue("balanced");
  api.android.powerPolicySet.mockResolvedValue(undefined);
  api.probe.mockImplementation(
    async (_args: unknown, onSample: (s: ProbeSampleDto) => void, onEnd: OnEnd) => {
      sample = onSample;
      probeEnd = onEnd;
      return { id: 3, cancel };
    },
  );
  status.set(running({ tags: ["desktop"] }));
  members.set([DESK, PIXEL]);
  toasts.set([]);
  theme.set("system");
});

async function mount() {
  const r = render(Settings);
  for (let i = 0; i < 4; i++) await tick();
  return r;
}

async function startProbe() {
  await fireEvent.click(screen.getByRole("radio", { name: "Pixel" }));
  await fireEvent.click(screen.getByRole("button", { name: "Measure latency" }));
  await tick();
  await tick();
}

it("settings_lists_only_the_spec_keys", async () => {
  await mount();
  expect(screen.getByRole("heading", { name: "This device" })).toBeInTheDocument();
  expect(screen.getByRole("heading", { name: "Port mappings" })).toBeInTheDocument();
  expect(screen.getByRole("heading", { name: "Relay" })).toBeInTheDocument();
  expect(screen.getByRole("heading", { name: "Appearance" })).toBeInTheDocument();
  expect(screen.queryByText(/log_level|discovery|windows_user_sid|max_memory/)).toBeNull();
  // Which daemon this app talks to.
  expect(screen.getByText("System service")).toBeInTheDocument();
});

it("settings_saves_name_and_tags_through_set_info", async () => {
  await mount();
  const name = screen.getByLabelText("Device name");
  expect(name).toHaveValue("Desk");
  await fireEvent.input(name, { target: { value: "  Big desk " } });
  await fireEvent.input(screen.getByLabelText("Add a tag"), { target: { value: "office" } });
  await fireEvent.click(screen.getByRole("button", { name: "Add tag" }));
  await fireEvent.click(screen.getByRole("button", { name: "Remove desktop" }));
  await fireEvent.click(screen.getByRole("button", { name: "Save device" }));
  await tick();
  expect(api.setInfo).toHaveBeenCalledWith({ name: "Big desk", tags: ["office"] });
  await tick();
  await tick();
  expect(get(toasts).map((t) => t.message)).toContain("Device saved");
});

it("config_restart_required_notice", async () => {
  await mount();
  // Relay: a custom URL.
  await fireEvent.click(screen.getByRole("radio", { name: "Custom" }));
  const url = screen.getByLabelText("Relay URL");
  await fireEvent.input(url, { target: { value: "http://plain" } });
  await fireEvent.click(screen.getByRole("button", { name: "Save relay" }));
  expect(screen.getByText(/Use an https:\/\/ address/)).toBeInTheDocument();
  expect(api.configSet).not.toHaveBeenCalled();
  await fireEvent.input(url, { target: { value: "https://relay.example" } });
  await fireEvent.click(screen.getByRole("button", { name: "Save relay" }));
  await tick();
  expect(api.configSet).toHaveBeenCalledWith("relay", "https://relay.example");
  await tick();
  expect(screen.getAllByText("Restart Meshnode to apply").length).toBeGreaterThan(0);
});

it("config_saves_port_mappings_in_the_daemons_format", async () => {
  api.configSet.mockResolvedValue({ restartRequired: false });
  await mount();
  expect(screen.getByLabelText("Port 1")).toHaveValue("7801");
  expect(screen.getByLabelText("Service 1")).toHaveValue("sync");
  await fireEvent.click(screen.getByRole("button", { name: "Add a mapping" }));
  await fireEvent.input(screen.getByLabelText("Port 2"), { target: { value: "80" } });
  await fireEvent.input(screen.getByLabelText("Service 2"), { target: { value: "web" } });
  await fireEvent.click(screen.getByRole("button", { name: "Save port mappings" }));
  expect(screen.getByText("Use a port from 1024 to 65535")).toBeInTheDocument();
  await fireEvent.input(screen.getByLabelText("Port 2"), { target: { value: "7800" } });
  await fireEvent.click(screen.getByRole("button", { name: "Save port mappings" }));
  expect(screen.getByText("7800 is the main gateway port")).toBeInTheDocument();
  await fireEvent.input(screen.getByLabelText("Port 2"), { target: { value: "9000" } });
  await fireEvent.click(screen.getByRole("button", { name: "Save port mappings" }));
  await tick();
  expect(api.configSet).toHaveBeenCalledWith("gateway.ports", "7801=sync,9000=web");
  await tick();
  expect(screen.queryByText("Restart Meshnode to apply")).toBeNull();
});

it("config_error_shows_inline_with_its_hint", async () => {
  api.configSet.mockRejectedValue({
    code: "invalidArgument",
    message: "invalid service name",
    hint: "Check the name",
    reason: null,
  });
  await mount();
  await fireEvent.click(screen.getByRole("button", { name: "Save port mappings" }));
  await tick();
  await tick();
  const alert = screen.getByRole("alert");
  expect(alert).toHaveTextContent("invalid service name");
  expect(alert).toHaveTextContent("Check the name");
});

it("theme_choice_is_stored", async () => {
  await mount();
  await fireEvent.click(screen.getByRole("radio", { name: "Dark" }));
  expect(get(theme)).toBe("dark");
  expect(screen.getByRole("radio", { name: "Dark" })).toBeChecked();
});

it("probe_summary_percentiles", async () => {
  await mount();
  await startProbe();
  expect(api.probe).toHaveBeenCalledWith(
    { nodePrefix: "bb22", count: 20, intervalMs: 1000, timeoutMs: 30000 },
    expect.any(Function),
    expect.any(Function),
  );
  // 20 answers of 10..200 ms, and two that failed.
  for (let i = 1; i <= 20; i++) {
    sample({ seq: i, ms: i * 10, connectMs: null, error: null });
    if (i === 5 || i === 15) {
      sample({ seq: 100 + i, ms: null, connectMs: null, error: "timed out" });
    }
  }
  probeEnd(null);
  flushSync();
  const summary = screen.getByRole("list", { name: "Latency summary" });
  const value = (label: string) => within(summary).getByText(label).nextElementSibling;
  expect(value("p50")).toHaveTextContent("100 ms");
  expect(value("p95")).toHaveTextContent("190 ms");
  expect(value("max")).toHaveTextContent("200 ms");
  expect(value("failed")).toHaveTextContent("2");
  // The chart: one point per answer, one mark per failure, all SVG elements (no {@html}).
  const chart = screen.getByRole("img", { name: /Latency to Pixel/ });
  expect(chart.querySelectorAll("circle.sample")).toHaveLength(20);
  expect(chart.querySelectorAll(".fail")).toHaveLength(2);
  expect(chart.querySelector("[style]")).toBeNull();
});

it("probe_cancels_when_settings_closes", async () => {
  const { unmount } = await mount();
  await startProbe();
  unmount();
  expect(cancel).toHaveBeenCalledTimes(1);
});

it("probe_too_many_live_views_shows_inline", async () => {
  api.probe.mockRejectedValue({
    code: "busy",
    message: "Too many live views open",
    hint: null,
    reason: null,
  });
  await mount();
  await startProbe();
  expect(screen.getByRole("alert")).toHaveTextContent("Too many live views open");
});

it("probe_lost_connection_offers_resume", async () => {
  await mount();
  await startProbe();
  sample({ seq: 1, ms: 12, connectMs: null, error: null });
  probeEnd({ code: "daemonUnreachable", message: "gone", hint: null, reason: null });
  flushSync();
  expect(screen.getByText("Disconnected")).toBeInTheDocument();
  await fireEvent.click(screen.getByRole("button", { name: "Resume" }));
  await tick();
  expect(api.probe).toHaveBeenCalledTimes(2);
});

it("power_policy_android_only", async () => {
  const desktop = await mount();
  expect(screen.queryByRole("heading", { name: "Battery" })).toBeNull();
  expect(api.android.powerPolicyGet).not.toHaveBeenCalled();
  desktop.unmount();

  api.platform.mockResolvedValue("android");
  await mount();
  expect(screen.getByRole("heading", { name: "Battery" })).toBeInTheDocument();
  expect(api.desktop.connection).toHaveBeenCalledTimes(1); // only by the desktop mount
  const balanced = screen.getByRole("radio", { name: /Balanced/ });
  expect(balanced).toBeChecked();
  await fireEvent.click(screen.getByRole("radio", { name: /Always on/ }));
  await tick();
  expect(api.android.powerPolicySet).toHaveBeenCalledWith("always-on");
  await fireEvent.click(screen.getByRole("radio", { name: /Saver/ }));
  await tick();
  expect(api.android.powerPolicySet).toHaveBeenLastCalledWith("saver");
});

it("reset_identity_is_offered_only_once_removed", async () => {
  // The daemon refuses a reset unless the node was removed (mesh-host `reset_identity_by`).
  await mount();
  expect(screen.getByRole("button", { name: "Reset identity…" })).toBeDisabled();
  expect(screen.getByText(/remove it from another device/)).toBeInTheDocument();
});

it("reset_identity_requires_typed_name", async () => {
  status.set(running({ state: "removed", tags: ["desktop"] }));
  members.set([]);
  await mount();
  await fireEvent.click(screen.getByRole("button", { name: "Reset identity…" }));
  const dialog = screen.getByRole("dialog");
  const action = within(dialog).getByRole("button", { name: "Reset identity" });
  expect(action).toBeDisabled();
  const input = within(dialog).getByRole("textbox");
  await fireEvent.input(input, { target: { value: "desk" } });
  expect(action).toBeDisabled();
  await fireEvent.input(input, { target: { value: "Desk" } });
  expect(action).toBeEnabled();
  await fireEvent.click(action);
  await tick();
  expect(api.resetIdentity).toHaveBeenCalledTimes(1);
});

it("settings_shows_the_second_node_notice_when_both_run", async () => {
  api.desktop.connection.mockResolvedValue({ kind: "system", otherRunning: true });
  await mount();
  expect(screen.getByText(/second, separate node is running as you/)).toBeInTheDocument();
});

it("reset_identity_without_a_name_asks_for_the_word_reset", async () => {
  // `status_info` fills the name only while the daemon can read the node or meshd.toml;
  // the in-process status (Android) may leave it out.
  status.set(running({ state: "removed", name: null }));
  members.set([]);
  await mount();
  await fireEvent.click(screen.getByRole("button", { name: "Reset identity…" }));
  const dialog = screen.getByRole("dialog");
  expect(within(dialog).getByRole("heading")).toHaveTextContent("Reset this device's identity?");
  expect(dialog).toHaveTextContent("name isn't available right now, so type “reset”");
  const input = within(dialog).getByLabelText("Type “reset” to confirm");
  const action = within(dialog).getByRole("button", { name: "Reset identity" });
  await fireEvent.input(input, { target: { value: "this device" } });
  expect(action).toBeDisabled();
  await fireEvent.input(input, { target: { value: "reset" } });
  expect(action).toBeEnabled();
  await fireEvent.click(action);
  await tick();
  expect(api.resetIdentity).toHaveBeenCalledTimes(1);
});
