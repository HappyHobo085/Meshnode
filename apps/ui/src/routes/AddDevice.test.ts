import { fireEvent, render, screen } from "@testing-library/svelte";
import { tick } from "svelte";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { member, QR_SVG } from "./fixtures";

const api = vi.hoisted(() => ({
  invite: vi.fn(),
  platform: vi.fn(),
  status: vi.fn(),
  members: vi.fn(),
  services: vi.fn(),
}));
vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  api,
}));

import { members } from "../lib/stores";
import AddDevice from "./AddDevice.svelte";

const T0 = 1_700_000_000_000;
let t = T0;
const now = () => t;
const self = member({ id: "a", name: "Desk", isSelf: true });

beforeEach(() => {
  vi.clearAllMocks();
  // Only the countdown's interval is faked; promises and testing-library's polling stay real.
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
  t = T0;
  members.set([self]);
  api.invite.mockImplementation(async (_role: string, ttlS: number) => ({
    text: "mesh1qwertyuiopasdfghjkl",
    qrSvg: QR_SVG,
    expiresAtMs: t + ttlS * 1000,
  }));
});
afterEach(() => {
  vi.useRealTimers();
});

async function seconds(n: number) {
  t += n * 1000;
  vi.advanceTimersByTime(n * 1000);
  await tick();
}

it("add_device_shows_qr_and_counts_down", async () => {
  const { container } = render(AddDevice, { now });
  // Defaults: host, 15 minutes.
  expect(screen.getByRole("radio", { name: /host/i })).toBeChecked();
  expect(screen.getByRole("radio", { name: "15 min" })).toBeChecked();
  await fireEvent.click(screen.getByRole("button", { name: "Create invite" }));
  expect(api.invite).toHaveBeenCalledWith("host", 900);

  const timer = await screen.findByRole("timer");
  expect(timer).toHaveTextContent("15:00");
  // The QR is rebuilt from the viewBox and path data only.
  const qr = container.querySelector("svg.qr")!;
  expect(qr.getAttribute("viewBox")).toBe("0 0 29 29");
  expect(qr.querySelector("path")?.getAttribute("d")).toBe("M4 4h7v1h-7zM12 4h1v1h-1z");
  expect(screen.getByText("mesh1qwertyuiopasdfghjkl")).toBeInTheDocument();

  await seconds(1);
  expect(timer).toHaveTextContent("14:59");
});

it("add_device_uses_the_chosen_role_and_expiry", async () => {
  render(AddDevice, { now });
  await fireEvent.click(screen.getByRole("radio", { name: /client/i }));
  await fireEvent.click(screen.getByRole("radio", { name: "60 min" }));
  await fireEvent.click(screen.getByRole("button", { name: "Create invite" }));
  expect(api.invite).toHaveBeenCalledWith("client", 3600);
  expect(await screen.findByRole("timer")).toHaveTextContent("60:00");
});

it("add_device_detects_new_member", async () => {
  render(AddDevice, { now });
  await fireEvent.click(screen.getByRole("button", { name: "Create invite" }));
  await screen.findByRole("timer");
  // A poll that brings nothing new changes nothing.
  members.set([self]);
  await tick();
  expect(screen.queryByText(/joined/)).toBeNull();

  members.set([self, member({ id: "p", name: "Pixel", platform: "android", accentHue: 30 })]);
  expect(await screen.findByText("Pixel joined")).toBeInTheDocument();
  expect(screen.getByRole("article")).toHaveTextContent("Pixel");
  expect(screen.queryByRole("timer")).toBeNull();
});

it("add_device_expired_offers_new_invite", async () => {
  render(AddDevice, { now });
  await fireEvent.click(screen.getByRole("radio", { name: "5 min" }));
  await fireEvent.click(screen.getByRole("button", { name: "Create invite" }));
  const timer = await screen.findByRole("timer");
  await seconds(299);
  expect(timer).toHaveTextContent("0:01");
  await seconds(1);
  expect(await screen.findByText("Invite expired")).toBeInTheDocument();
  expect(screen.queryByRole("timer")).toBeNull();

  await fireEvent.click(screen.getByRole("button", { name: "New invite" }));
  expect(api.invite).toHaveBeenCalledTimes(2);
  expect(api.invite).toHaveBeenLastCalledWith("host", 300);
  expect(await screen.findByRole("timer")).toHaveTextContent("5:00");
});

it("add_device_shows_the_error_and_hint_when_invite_fails", async () => {
  api.invite.mockRejectedValue({
    code: "notRunning",
    message: "The node is not running",
    hint: "Start it from the banner",
    reason: null,
  });
  render(AddDevice, { now });
  await fireEvent.click(screen.getByRole("button", { name: "Create invite" }));
  const alert = await screen.findByRole("alert");
  expect(alert).toHaveTextContent("The node is not running");
  expect(alert).toHaveTextContent("Start it from the banner");
});

it("add_device_keeps_only_path_data_from_the_qr", async () => {
  api.invite.mockResolvedValue({
    text: "mesh1x",
    qrSvg:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 9 9"><path d="M0 0h1v1z" onload="alert(1)"/><script>alert(2)</script></svg>',
    expiresAtMs: t + 900_000,
  });
  const { container } = render(AddDevice, { now });
  await fireEvent.click(screen.getByRole("button", { name: "Create invite" }));
  await screen.findByRole("timer");
  const qr = container.querySelector("svg.qr")!;
  expect(qr.querySelector("script")).toBeNull();
  expect(qr.querySelector("path")?.getAttribute("onload")).toBeNull();
  expect(qr.querySelector("path")?.getAttribute("d")).toBe("M0 0h1v1z");
});
