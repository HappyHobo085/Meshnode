import { fireEvent, render, screen } from "@testing-library/svelte";
import { beforeEach, expect, it, vi } from "vitest";
import { running } from "./fixtures";

const api = vi.hoisted(() => ({
  init: vi.fn(),
  platform: vi.fn(),
  status: vi.fn(),
  members: vi.fn(),
  services: vi.fn(),
}));
vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  api,
}));

import FirstRun from "./FirstRun.svelte";

beforeEach(() => {
  vi.clearAllMocks();
  api.platform.mockResolvedValue("desktop-windows");
  api.status.mockResolvedValue(running());
  api.members.mockResolvedValue([]);
  api.services.mockResolvedValue([]);
  location.hash = "#/first-run";
});

it("first_run_create_calls_init_with_name", async () => {
  api.init.mockResolvedValue(running({ name: "Desk" }));
  render(FirstRun);
  const name = screen.getByLabelText("Device name");
  // The platform default arrives first; the user's own name then wins.
  await vi.waitFor(() => expect(name).toHaveValue("Windows PC"));
  await fireEvent.input(name, { target: { value: "  Desk  " } });
  await fireEvent.click(screen.getByRole("button", { name: "Create mesh" }));
  expect(api.init).toHaveBeenCalledWith("Desk");
  await vi.waitFor(() => expect(location.hash).toBe("#/"));
});

it("first_run_shows_the_error_and_hint_when_init_fails", async () => {
  api.init.mockRejectedValue({
    code: "wrongState",
    message: "This device already belongs to a mesh",
    hint: "Reset its identity in Settings first",
    reason: null,
  });
  render(FirstRun);
  await fireEvent.input(screen.getByLabelText("Device name"), { target: { value: "Desk" } });
  await fireEvent.click(screen.getByRole("button", { name: "Create mesh" }));
  const alert = await screen.findByRole("alert");
  expect(alert).toHaveTextContent("This device already belongs to a mesh");
  expect(alert).toHaveTextContent("Reset its identity in Settings first");
});

it("first_run_join_goes_to_join", async () => {
  render(FirstRun);
  await fireEvent.click(screen.getByRole("button", { name: "Join a mesh" }));
  expect(location.hash).toBe("#/join");
  expect(api.init).not.toHaveBeenCalled();
});
