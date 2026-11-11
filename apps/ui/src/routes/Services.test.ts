import { fireEvent, render, screen, within } from "@testing-library/svelte";
import { beforeEach, expect, it, vi } from "vitest";
import { member, running, service } from "./fixtures";

const api = vi.hoisted(() => ({
  platform: vi.fn(),
  status: vi.fn(),
  members: vi.fn(),
  services: vi.fn(),
}));
vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  api,
}));

import { members, services, status } from "../lib/stores";
import Services from "./Services.svelte";

const DESK = member({ id: "aa11", name: "Desk", isSelf: true, accentHue: 30 });
const PIXEL = member({ id: "bb22", name: "Pixel", platform: "android", accentHue: 250 });

beforeEach(() => {
  vi.clearAllMocks();
  api.platform.mockResolvedValue("desktop-windows");
  status.set(running());
  members.set([DESK, PIXEL]);
  services.set([]);
  location.hash = "#/services";
});

it("services_lists_hosts_as_monograms_and_sizes", () => {
  services.set([
    service({
      name: "blog",
      kind: "static",
      version: 3,
      placedHosts: ["aa11", "bb22"],
      local: { kind: "ready", version: 3 },
      dataBytes: 12_400_000,
    }),
    service({
      name: "notes-api",
      kind: "wasm",
      version: 1,
      // A host that isn't in `members` (yet) still gets a tile, from its id.
      placedHosts: ["cc33"],
      local: { kind: "notPlaced" },
      dataBytes: 850,
    }),
  ]);
  render(Services);

  const rows = screen.getAllByRole("listitem");
  expect(rows).toHaveLength(2);
  const [blog, notes] = rows as [HTMLElement, HTMLElement];

  expect(within(blog).getByRole("link", { name: /blog/ })).toHaveAttribute(
    "href",
    "#/services/blog",
  );
  expect(blog).toHaveTextContent("Static site");
  expect(blog).toHaveTextContent("v3");
  expect(blog).toHaveTextContent("12 MB");
  expect(blog).toHaveTextContent("Running here");
  // One monogram per host, in that host's own colour; the names are there as text too.
  const tiles = [...blog.querySelectorAll<HTMLElement>(".mono-tile")];
  expect(tiles.map((t) => t.textContent?.trim())).toEqual(["DE", "PI"]);
  expect(tiles.map((t) => t.style.getPropertyValue("--h"))).toEqual(["30", "250"]);
  expect(blog).toHaveTextContent("Runs on Desk and Pixel");

  expect(notes).toHaveTextContent("WebAssembly");
  expect(notes).toHaveTextContent("850 B");
  expect(notes).toHaveTextContent("Not on this device");
  const unknown = notes.querySelector<HTMLElement>(".mono-tile")!;
  // accent hue as in Rust: the id's first two bytes, mod 360 (0xcc33 = 52275 → 75).
  expect(unknown.style.getPropertyValue("--h")).toBe("75");
});

it("services_empty_state_offers_deploy", async () => {
  render(Services);
  expect(screen.getByText(/no services yet/i)).toBeInTheDocument();
  const deploy = screen.getAllByRole("button", { name: /deploy/i })[0]!;
  await fireEvent.click(deploy);
  expect(location.hash).toBe("#/deploy");
});

it("services_shows_a_failed_service_with_its_error", () => {
  services.set([
    service({ name: "shop", local: { kind: "failed", error: "module trapped on start" } }),
  ]);
  render(Services);
  const row = screen.getByRole("listitem");
  expect(row).toHaveTextContent("Failed here");
  expect(row).toHaveTextContent("module trapped on start");
});

it("services_folds_many_hosts_behind_a_count", () => {
  const many = Array.from({ length: 7 }, (_, i) =>
    member({ id: `${i}0aa`, name: `Host ${i}`, accentHue: i * 40 }),
  );
  members.set(many);
  services.set([service({ name: "wide", placedHosts: many.map((m) => m.id) })]);
  render(Services);
  const row = screen.getByRole("listitem");
  expect(row.querySelectorAll(".mono-tile")).toHaveLength(4);
  expect(row).toHaveTextContent("+3");
});
