import { fireEvent, render, screen } from "@testing-library/svelte";
import { beforeEach, expect, it, vi } from "vitest";
import { running } from "./fixtures";

const api = vi.hoisted(() => ({
  join: vi.fn(),
  platform: vi.fn(),
  status: vi.fn(),
  members: vi.fn(),
  services: vi.fn(),
  android: { scan: vi.fn() },
}));
vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  api,
}));

import Join from "./Join.svelte";

beforeEach(() => {
  vi.clearAllMocks();
  api.status.mockResolvedValue(running());
  api.members.mockResolvedValue([]);
  api.services.mockResolvedValue([]);
  location.hash = "#/join";
});

it("join_paste_trims_and_shows_hint_on_error", async () => {
  api.platform.mockResolvedValue("desktop-windows");
  api.join.mockRejectedValue({
    code: "joinRefused",
    message: "join refused: expired",
    hint: "Create a new invite on the other device",
    reason: "expired",
  });
  render(Join);
  const ticket = await screen.findByLabelText("Invite");
  await fireEvent.input(ticket, { target: { value: "  mesh1abcdef \n" } });
  await fireEvent.input(screen.getByLabelText("Device name"), { target: { value: " Laptop " } });
  await fireEvent.click(screen.getByRole("button", { name: "Join mesh" }));
  expect(api.join).toHaveBeenCalledWith("mesh1abcdef", "Laptop");
  const alert = await screen.findByRole("alert");
  expect(alert).toHaveTextContent("This invite has expired");
  expect(alert).toHaveTextContent("Create a new invite on the other device");
  expect(location.hash).toBe("#/join");
});

it("join_success_goes_home", async () => {
  api.platform.mockResolvedValue("desktop-linux");
  api.join.mockResolvedValue(running());
  render(Join);
  await fireEvent.input(await screen.findByLabelText("Invite"), { target: { value: "mesh1a" } });
  await fireEvent.input(screen.getByLabelText("Device name"), { target: { value: "Box" } });
  await fireEvent.click(screen.getByRole("button", { name: "Join mesh" }));
  await vi.waitFor(() => expect(location.hash).toBe("#/"));
});

it("join_scan_only_on_android", async () => {
  api.platform.mockResolvedValue("desktop-windows");
  const desktop = render(Join);
  expect(await screen.findByLabelText("Invite")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /scan/i })).toBeNull();
  desktop.unmount();

  api.platform.mockResolvedValue("android");
  render(Join);
  expect(await screen.findByRole("button", { name: "Scan QR code" })).toBeInTheDocument();
  // The paste box is one tap away.
  expect(screen.queryByLabelText("Invite")).toBeNull();
  await fireEvent.click(screen.getByRole("button", { name: "Paste instead" }));
  expect(screen.getByLabelText("Invite")).toBeInTheDocument();
});

it("join_scan_joins_with_the_scanned_ticket", async () => {
  api.platform.mockResolvedValue("android");
  api.android.scan.mockResolvedValue(" mesh1scanned ");
  api.join.mockResolvedValue(running());
  render(Join);
  await fireEvent.input(screen.getByLabelText("Device name"), { target: { value: "Pixel" } });
  await fireEvent.click(await screen.findByRole("button", { name: "Scan QR code" }));
  await vi.waitFor(() => expect(api.join).toHaveBeenCalledWith("mesh1scanned", "Pixel"));
});

it("join_scan_needs_a_name", async () => {
  api.platform.mockResolvedValue("android");
  render(Join);
  const scan = await screen.findByRole("button", { name: "Scan QR code" });
  await fireEvent.input(screen.getByLabelText("Device name"), { target: { value: "  " } });
  expect(scan).toBeDisabled();
  await fireEvent.input(screen.getByLabelText("Device name"), { target: { value: "Pixel" } });
  expect(scan).toBeEnabled();
});
