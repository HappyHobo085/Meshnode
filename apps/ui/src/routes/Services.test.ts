import { fireEvent, render, screen, within } from "@testing-library/svelte";
import { beforeEach, expect, it, vi } from "vitest";
import { member, running, service } from "./fixtures";

const api = vi.hoisted(() => ({
  platform: vi.fn(),
  status: vi.fn(),
  members: vi.fn(),
  services: vi.fn(),
  desktop: { pickFolder: vi.fn(), deployStaticDir: vi.fn() },
}));
vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  api,
}));

import { get } from "svelte/store";
import { deployState, resetDeployForTests } from "../lib/deploy";
import { members, services, status, toasts } from "../lib/stores";
import Services from "./Services.svelte";

const DESK = member({ id: "aa11", name: "Desk", isSelf: true, accentHue: 30 });
const PIXEL = member({ id: "bb22", name: "Pixel", platform: "android", accentHue: 250 });

beforeEach(() => {
  vi.clearAllMocks();
  api.platform.mockResolvedValue("desktop-windows");
  status.set(running());
  members.set([DESK, PIXEL]);
  services.set([]);
  toasts.set([]);
  resetDeployForTests();
  api.status.mockResolvedValue(running());
  api.members.mockResolvedValue([DESK, PIXEL]);
  api.services.mockResolvedValue([]);
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

/** Opens Deploy over the list and starts "blog"; the upload stays pending. */
async function startBlogFromTheList() {
  api.desktop.pickFolder.mockResolvedValue("C:/Sites/blog");
  const done: { finish: (s: unknown) => void; fail: (e: unknown) => void } = {
    finish: () => {},
    fail: () => {},
  };
  api.desktop.deployStaticDir.mockImplementation(
    (_p: string, _n: string, _o: unknown, onProgress: (n: number) => void) => {
      onProgress(2_500_000);
      return new Promise((resolve, reject) => {
        done.finish = resolve;
        done.fail = reject;
      });
    },
  );
  const view = render(Services, { deploying: true });
  await fireEvent.click(await screen.findByRole("button", { name: /site folder/i }));
  await fireEvent.click(await screen.findByRole("button", { name: "Next" }));
  await fireEvent.click(screen.getByRole("button", { name: "Deploy blog" }));
  expect(await screen.findByRole("progressbar")).toBeInTheDocument();
  return { view, done };
}

const TOO_LARGE = {
  code: "invalidArgument",
  message: "file too large: C:/Sites/blog/big.bin",
  hint: "Leave large files out of the site folder.",
  reason: null,
};

it("services_shows_a_deploy_that_failed_after_its_sheet_closed", async () => {
  const { view, done } = await startBlogFromTheList();
  await view.rerender({ deploying: false });
  // Still going: the list says so.
  expect(await screen.findByRole("status")).toHaveTextContent("Deploying blog, 2.5 MB read");

  done.fail(TOO_LARGE);
  const row = await screen.findByRole("alert");
  expect(row).toHaveTextContent("Couldn't deploy blog");
  expect(row).toHaveTextContent("big.bin");
  expect(row).toHaveTextContent("Leave large files out of the site folder.");
  expect(get(toasts)).toEqual([]);

  // The reopened sheet shows it too (and the list row steps aside meanwhile).
  await view.rerender({ deploying: true });
  const inSheet = await screen.findByRole("alert");
  expect(screen.getByRole("dialog")).toContainElement(inSheet);
  expect(inSheet).toHaveTextContent("big.bin");

  // Closed again: the row is back until dismissed.
  await view.rerender({ deploying: false });
  await fireEvent.click(await screen.findByRole("button", { name: "Dismiss" }));
  expect(screen.queryByRole("alert")).toBeNull();
  expect(get(deployState)).toBeNull();
});

it("services_left_mid_deploy_shows_the_result_on_return", async () => {
  // Failure while the screen is gone (sidebar, tabs or Back unmount it).
  const first = await startBlogFromTheList();
  first.view.unmount();
  first.done.fail(TOO_LARGE);
  await vi.waitFor(() => expect(get(deployState)?.status).toBe("failed"));
  const back = render(Services);
  expect(await screen.findByRole("alert")).toHaveTextContent("Couldn't deploy blog");
  await fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
  back.unmount();

  // Success while the screen is gone: the toast, and nothing left over.
  const second = await startBlogFromTheList();
  second.view.unmount();
  second.done.finish(service({ name: "blog" }));
  await vi.waitFor(() => expect(get(toasts).map((t) => t.message)).toContain("Deployed blog"));
  render(Services);
  expect(screen.queryByRole("alert")).toBeNull();
  expect(screen.queryByRole("status")).toBeNull();
});
