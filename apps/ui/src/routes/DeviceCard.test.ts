import { fireEvent, render, screen, within } from "@testing-library/svelte";
import { tick } from "svelte";
import { get } from "svelte/store";
import { beforeEach, expect, it, vi } from "vitest";
import { member, running } from "./fixtures";

const api = vi.hoisted(() => ({
  platform: vi.fn(),
  status: vi.fn(),
  members: vi.fn(),
  services: vi.fn(),
  remove: vi.fn(),
}));
vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  api,
}));

import { toasts } from "../lib/stores";
import DeviceCard from "./DeviceCard.svelte";

const DESK = member({ id: "aa11", name: "Desk", isSelf: true });
const PIXEL = member({ id: "bb22", name: "Pixel", platform: "android", accentHue: 250 });

beforeEach(() => {
  vi.clearAllMocks();
  api.remove.mockResolvedValue(undefined);
  api.status.mockResolvedValue(running());
  api.members.mockResolvedValue([DESK]);
  api.services.mockResolvedValue([]);
  toasts.set([]);
});

async function openRemove() {
  await fireEvent.click(screen.getByRole("button", { name: "More actions for Pixel" }));
  await fireEvent.click(screen.getByRole("menuitem", { name: "Remove device…" }));
  return screen.getByRole("dialog");
}

it("remove_device_requires_the_typed_name_and_says_it_cannot_rejoin", async () => {
  render(DeviceCard, { member: PIXEL });
  const dialog = await openRemove();
  expect(dialog).toHaveTextContent(/a removed device can never rejoin with the same identity/i);
  const action = within(dialog).getByRole("button", { name: "Remove device" });
  expect(action).toBeDisabled();
  const input = within(dialog).getByRole("textbox");
  await fireEvent.input(input, { target: { value: "pixel" } });
  expect(action).toBeDisabled();
  await fireEvent.input(input, { target: { value: "Pixel" } });
  await fireEvent.click(action);
  await tick();
  expect(api.remove).toHaveBeenCalledWith("bb22");
  for (let i = 0; i < 4; i++) await tick();
  // refreshNow() polled the daemon again.
  expect(api.status).toHaveBeenCalled();
  expect(get(toasts).map((t) => t.message)).toContain("Removed Pixel");
});

it("remove_is_not_offered_for_this_device", () => {
  render(DeviceCard, { member: DESK });
  expect(screen.queryByRole("button", { name: /More actions/ })).toBeNull();
});

it("remove_last_host_shows_its_hint_inline", async () => {
  api.remove.mockRejectedValue({
    code: "lastHost",
    message: "Pixel is the last host",
    hint: "Add another host device before removing this one.",
    reason: null,
  });
  render(DeviceCard, { member: PIXEL });
  const dialog = await openRemove();
  await fireEvent.input(within(dialog).getByRole("textbox"), { target: { value: "Pixel" } });
  await fireEvent.click(within(dialog).getByRole("button", { name: "Remove device" }));
  await tick();
  await tick();
  const alert = within(screen.getByRole("dialog")).getByRole("alert");
  expect(alert).toHaveTextContent("Pixel is the last host");
  expect(alert).toHaveTextContent("Add another host device before removing this one.");
});

it("card_menu_closes_on_escape", async () => {
  render(DeviceCard, { member: PIXEL });
  const button = screen.getByRole("button", { name: "More actions for Pixel" });
  await fireEvent.click(button);
  expect(button).toHaveAttribute("aria-expanded", "true");
  await fireEvent.keyDown(screen.getByRole("menu"), { key: "Escape" });
  expect(screen.queryByRole("menu")).toBeNull();
  expect(button).toHaveAttribute("aria-expanded", "false");
});
