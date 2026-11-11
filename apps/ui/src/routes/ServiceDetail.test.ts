import { fireEvent, render, screen } from "@testing-library/svelte";
import { get } from "svelte/store";
import { beforeEach, expect, it, vi } from "vitest";
import { member, running, service } from "./fixtures";

const api = vi.hoisted(() => ({
  platform: vi.fn(),
  status: vi.fn(),
  members: vi.fn(),
  services: vi.fn(),
  setPlacement: vi.fn(),
  setOutbound: vi.fn(),
  setLimits: vi.fn(),
  undeploy: vi.fn(),
  purge: vi.fn(),
}));
vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  api,
}));

import { members, services, status, toasts } from "../lib/stores";
import ServiceDetail from "./ServiceDetail.svelte";

const DESK = member({ id: "aa11", name: "Desk", isSelf: true, tags: ["desktop"], accentHue: 30 });
const PIXEL = member({
  id: "bb22",
  name: "Pixel",
  platform: "android",
  tags: ["phone"],
  accentHue: 250,
});
const BLOG = service({
  name: "blog",
  kind: "wasm",
  version: 2,
  placedHosts: ["aa11", "bb22"],
  dataBytes: 4_200_000,
});

beforeEach(() => {
  vi.clearAllMocks();
  for (const f of [api.setPlacement, api.setOutbound, api.setLimits, api.undeploy, api.purge]) {
    f.mockResolvedValue(undefined);
  }
  api.platform.mockResolvedValue("desktop-windows");
  api.status.mockResolvedValue(running());
  api.members.mockResolvedValue([DESK, PIXEL]);
  api.services.mockResolvedValue([BLOG]);
  status.set(running());
  members.set([DESK, PIXEL]);
  services.set([BLOG]);
  toasts.set([]);
});

function open(onclose = vi.fn()) {
  render(ServiceDetail, { name: "blog", open: true, onclose });
  return onclose;
}

it("service_detail_shows_the_hosts_with_their_names", () => {
  open();
  const dialog = screen.getByRole("dialog", { name: "blog" });
  expect(dialog).toHaveTextContent("Desk");
  expect(dialog).toHaveTextContent("Pixel");
  expect(dialog).toHaveTextContent("4.2 MB");
  expect(dialog.querySelectorAll(".hosts .mono-tile")).toHaveLength(2);
});

it("placement_editor_round_trip", async () => {
  open();
  const save = screen.getByRole("button", { name: "Save placement" });
  expect(screen.getByRole("radio", { name: "All hosts" })).toBeChecked();
  // Nothing changed yet.
  expect(save).toBeDisabled();

  await fireEvent.click(screen.getByRole("radio", { name: "Hosts with tags" }));
  // Tags mode without any tag chosen isn't a placement yet.
  expect(save).toBeDisabled();
  await fireEvent.click(screen.getByRole("button", { name: "desktop" }));
  expect(screen.getByRole("button", { name: "desktop" })).toHaveAttribute("aria-pressed", "true");
  expect(save).toBeEnabled();
  await fireEvent.click(save);

  expect(api.setPlacement).toHaveBeenCalledWith("blog", { kind: "tags", tags: ["desktop"] });
  await vi.waitFor(() => expect(get(toasts).map((t) => t.message)).toContain("Placement saved"));
});

it("placement_editor_can_choose_hosts_by_name", async () => {
  open();
  await fireEvent.click(screen.getByRole("radio", { name: "Chosen hosts" }));
  await fireEvent.click(screen.getByRole("button", { name: "Pixel" }));
  await fireEvent.click(screen.getByRole("button", { name: "Save placement" }));
  expect(api.setPlacement).toHaveBeenCalledWith("blog", { kind: "nodes", ids: ["bb22"] });
});

it("placement_shows_the_error_and_hint_when_saving_fails", async () => {
  api.setPlacement.mockRejectedValue({
    code: "invalidArgument",
    message: "placement tag too long",
    hint: "Tags are up to 32 characters.",
    reason: null,
  });
  open();
  await fireEvent.click(screen.getByRole("radio", { name: "Hosts with tags" }));
  await fireEvent.click(screen.getByRole("button", { name: "phone" }));
  await fireEvent.click(screen.getByRole("button", { name: "Save placement" }));
  const alert = await screen.findByRole("alert");
  expect(alert).toHaveTextContent("placement tag too long");
  expect(alert).toHaveTextContent("Tags are up to 32 characters.");
});

it("outbound_editor_rejects_bad_entry", async () => {
  open();
  const input = screen.getByLabelText("Add an address");
  await fireEvent.input(input, { target: { value: "no-port" } });
  await fireEvent.click(screen.getByRole("button", { name: "Add address" }));
  expect(screen.getByRole("alert")).toHaveTextContent("Use host:port");
  expect(screen.getByRole("button", { name: "Save outbound" })).toBeDisabled();

  for (const bad of ["example.com:", ":443", "example.com:70000", "two words:80"]) {
    await fireEvent.input(input, { target: { value: bad } });
    await fireEvent.click(screen.getByRole("button", { name: "Add address" }));
    expect(screen.getByRole("alert")).toHaveTextContent("Use host:port");
  }

  await fireEvent.input(input, { target: { value: " api.example.com:443 " } });
  await fireEvent.click(screen.getByRole("button", { name: "Add address" }));
  expect(screen.queryByRole("alert")).toBeNull();
  expect(screen.getByText("api.example.com:443")).toBeInTheDocument();
  await fireEvent.click(screen.getByRole("button", { name: "Save outbound" }));
  expect(api.setOutbound).toHaveBeenCalledWith("blog", ["api.example.com:443"]);
});

it("outbound_editor_removes_an_entry", async () => {
  services.set([{ ...BLOG, outbound: ["db.lan:5432", "api.example.com:443"] }]);
  open();
  await fireEvent.click(screen.getByRole("button", { name: "Remove db.lan:5432" }));
  await fireEvent.click(screen.getByRole("button", { name: "Save outbound" }));
  expect(api.setOutbound).toHaveBeenCalledWith("blog", ["api.example.com:443"]);
});

it("limits_save_in_the_units_the_daemon_takes", async () => {
  open();
  await fireEvent.input(screen.getByLabelText("Memory (MB)"), { target: { value: "128" } });
  await fireEvent.input(screen.getByLabelText("Request timeout (s)"), {
    target: { value: "2.5" },
  });
  await fireEvent.input(screen.getByLabelText("Largest request (MiB)"), {
    target: { value: "16" },
  });
  await fireEvent.click(screen.getByRole("button", { name: "Save limits" }));
  expect(api.setLimits).toHaveBeenCalledWith("blog", {
    memoryMb: 128,
    requestTimeoutMs: 2500,
    maxBodyBytes: 16 * 1024 * 1024,
  });
});

it("limits_out_of_range_are_refused_before_sending", async () => {
  open();
  await fireEvent.input(screen.getByLabelText("Memory (MB)"), { target: { value: "4096" } });
  await fireEvent.click(screen.getByRole("button", { name: "Save limits" }));
  expect(api.setLimits).not.toHaveBeenCalled();
  expect(screen.getByRole("alert")).toHaveTextContent("1 to 1024");
});

it("purge_requires_typed_name", async () => {
  open();
  await fireEvent.click(screen.getByRole("button", { name: "Purge data…" }));
  const confirm = screen.getByRole("dialog", { name: "Purge blog's data?" });
  expect(confirm).toHaveTextContent("purge hides all of this service's data on every device");
  const action = screen.getByRole("button", { name: "Purge data" });
  expect(action).toBeDisabled();
  const typed = screen.getByLabelText("Type “blog” to confirm");
  await fireEvent.input(typed, { target: { value: "Blog" } });
  expect(action).toBeDisabled();
  await fireEvent.input(typed, { target: { value: "blog" } });
  expect(action).toBeEnabled();
  await fireEvent.click(action);
  expect(api.purge).toHaveBeenCalledWith("blog");
  await vi.waitFor(() => expect(get(toasts).map((t) => t.message)).toContain("Purged blog's data"));
});

it("undeploy_requires_typed_name_and_says_the_data_stays", async () => {
  open();
  // After the undeploy, the next poll no longer lists it.
  api.undeploy.mockImplementation(async () => {
    api.services.mockResolvedValue([]);
  });
  await fireEvent.click(screen.getByRole("button", { name: "Undeploy…" }));
  const confirm = screen.getByRole("dialog", { name: "Undeploy blog?" });
  expect(confirm).toHaveTextContent("Its data stays on your devices until you purge it.");
  const action = screen.getByRole("button", { name: "Undeploy" });
  expect(action).toBeDisabled();
  await fireEvent.input(screen.getByLabelText("Type “blog” to confirm"), {
    target: { value: "blog" },
  });
  await fireEvent.click(action);
  expect(api.undeploy).toHaveBeenCalledWith("blog");
  await vi.waitFor(() => expect(get(toasts).map((t) => t.message)).toContain("Undeployed blog"));
  // The sheet stays, so the data that stayed behind can still be purged.
  await vi.waitFor(() =>
    expect(screen.getByRole("dialog")).toHaveTextContent("blog isn't deployed"),
  );
  expect(screen.getByRole("button", { name: "Purge data…" })).toBeInTheDocument();
});

it("service_detail_for_a_missing_service_says_so_and_offers_purge", async () => {
  services.set([]);
  render(ServiceDetail, { name: "gone", open: true, onclose: vi.fn() });
  expect(screen.getByRole("dialog")).toHaveTextContent("gone isn't deployed");
  expect(screen.queryByRole("button", { name: "Save placement" })).toBeNull();
  await fireEvent.click(screen.getByRole("button", { name: "Purge data…" }));
  expect(screen.getByRole("dialog", { name: "Purge gone's data?" })).toBeInTheDocument();
});
