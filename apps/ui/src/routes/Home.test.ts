import { fireEvent, render, screen, within } from "@testing-library/svelte";
import { readFileSync } from "node:fs";
import { get } from "svelte/store";
import { beforeEach, expect, it, vi } from "vitest";
import { member, running } from "./fixtures";
import homeSource from "./Home.svelte?raw";

const api = vi.hoisted(() => ({
  platform: vi.fn(),
  status: vi.fn(),
  members: vi.fn(),
  services: vi.fn(),
  desktop: { openUrl: vi.fn() },
}));
vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  api,
}));

import { members, status, toasts } from "../lib/stores";
import Home from "./Home.svelte";

// Vitest stubs `.css` imports (even `?raw`), so the stylesheet is read from disk.
const baseCss = readFileSync(`${process.cwd()}/src/lib/design/base.css`, "utf8");

beforeEach(() => {
  vi.clearAllMocks();
  api.platform.mockResolvedValue("desktop-windows");
  api.desktop.openUrl.mockResolvedValue(undefined);
  toasts.set([]);
});

it("home_pins_self_first_and_shows_counts", () => {
  status.set(running({ membersTotal: 4, membersOnline: 3 }));
  members.set([
    member({ id: "b", name: "Pixel", platform: "android", accentHue: 30 }),
    member({ id: "c", name: "Laptop", online: false, lastSeenMs: Date.now() - 120_000 }),
    member({ id: "a", name: "Desk", isSelf: true }),
    member({ id: "d", name: "Server", platform: "linux" }),
  ]);
  render(Home);
  expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent("3 of 4 devices online");
  const cards = screen.getAllByRole("article");
  expect(cards).toHaveLength(4);
  expect(within(cards[0]!).getByText("Desk")).toBeInTheDocument();
  expect(within(cards[0]!).getByText("This device")).toBeInTheDocument();
  expect(within(cards[1]!).getByText("Pixel")).toBeInTheDocument();
  // Status is text, not colour alone.
  const laptop = cards.find((c) => within(c).queryByText("Laptop"))!;
  expect(laptop).toHaveTextContent("Offline");
  expect(laptop).toHaveTextContent("last seen 2 min ago");
});

it("home_renders_fifty_members", () => {
  const name = (i: number) => `Device-${String(i).padStart(2, "0")}-${"x".repeat(20)}`; // 30 chars
  const tags = Array.from({ length: 20 }, (_, t) => `tag-number-${t}`);
  status.set(running({ membersTotal: 50, membersOnline: 50 }));
  members.set(
    Array.from({ length: 50 }, (_, i) =>
      member({ id: `id${i}`, name: name(i), isSelf: i === 0, tags, accentHue: (i * 37) % 360 }),
    ),
  );
  document.body.style.width = "360px";
  render(Home);
  const cards = screen.getAllByRole("article");
  expect(cards).toHaveLength(50);
  expect(name(7)).toHaveLength(30);
  for (const card of cards) {
    // Long names truncate with an ellipsis; tags wrap onto new lines inside the card.
    expect(card.querySelector(".name")).toHaveClass("truncate");
    expect(card.querySelector(".tags")).toHaveClass("wrap");
  }
  // The utilities really do what their names say.
  expect(baseCss).toMatch(/\.truncate\s*\{[^}]*text-overflow:\s*ellipsis/);
  expect(baseCss).toMatch(/\.truncate\s*\{[^}]*min-width:\s*0/);
  expect(baseCss).toMatch(/\.wrap\s*\{[^}]*flex-wrap:\s*wrap/);
  expect(baseCss).toMatch(/\.wrap\s*\{[^}]*overflow-wrap:\s*anywhere/);
  // The grid never asks for more than the container at 360 px.
  expect(homeSource).toMatch(/minmax\(min\(100%,/);
  document.body.style.width = "";
});

it("home_copy_gateway_url", async () => {
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
  status.set(
    running({ gatewayUrl: "http://127.0.0.1:7780", ports: [{ port: 8080, service: "blog" }] }),
  );
  members.set([member({ id: "a", name: "Desk", isSelf: true })]);
  render(Home);
  await fireEvent.click(screen.getByRole("button", { name: /copy gateway url/i }));
  expect(writeText).toHaveBeenCalledWith("http://127.0.0.1:7780");
  await vi.waitFor(() => expect(get(toasts).map((t) => t.message)).toContain("Gateway URL copied"));
  expect(screen.getByText("blog")).toBeInTheDocument();
  // Desktop can also open it in the browser.
  await fireEvent.click(await screen.findByRole("button", { name: /open in browser/i }));
  expect(api.desktop.openUrl).toHaveBeenCalledWith("http://127.0.0.1:7780");
});

it("home_gateway_is_copy_only_on_android", async () => {
  api.platform.mockResolvedValue("android");
  status.set(running({ gatewayUrl: "http://127.0.0.1:7780" }));
  members.set([member({ id: "a", name: "Pixel", isSelf: true, platform: "android" })]);
  render(Home);
  await vi.waitFor(() => expect(api.platform).toHaveBeenCalled());
  await new Promise((r) => setTimeout(r, 0));
  expect(screen.getByRole("button", { name: /copy gateway url/i })).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /open in browser/i })).toBeNull();
});

it("home_slides_in_only_members_that_appear_later", async () => {
  status.set(running({ membersTotal: 1, membersOnline: 1 }));
  members.set([member({ id: "a", name: "Desk", isSelf: true })]);
  render(Home);
  expect(screen.getByRole("article")).not.toHaveClass("arrive");
  members.set([
    member({ id: "a", name: "Desk", isSelf: true }),
    member({ id: "p", name: "Pixel", platform: "android" }),
  ]);
  await vi.waitFor(() => expect(screen.getAllByRole("article")).toHaveLength(2));
  const [desk, pixel] = screen.getAllByRole("article");
  expect(desk).not.toHaveClass("arrive");
  expect(pixel).toHaveClass("arrive");
});

it("home_open_gateway_on_a_wildcard_address_opens_loopback", async () => {
  status.set(running({ gatewayUrl: "http://0.0.0.0:7780" }));
  members.set([member({ id: "a", name: "Desk", isSelf: true })]);
  render(Home);
  await fireEvent.click(await screen.findByRole("button", { name: /open in browser/i }));
  expect(api.desktop.openUrl).toHaveBeenCalledWith("http://127.0.0.1:7780");
});

it("home_open_gateway_failure_is_shown_inline", async () => {
  api.desktop.openUrl.mockRejectedValue({
    code: "invalidArgument",
    message: "Only Meshnode's local addresses can be opened",
    hint: null,
    reason: null,
  });
  status.set(running({ gatewayUrl: "http://192.168.1.5:7780" }));
  members.set([member({ id: "a", name: "Desk", isSelf: true })]);
  render(Home);
  await fireEvent.click(await screen.findByRole("button", { name: /open in browser/i }));
  const alert = await screen.findByRole("alert");
  expect(alert).toHaveTextContent("Couldn't open http://192.168.1.5:7780");
  expect(alert).toHaveTextContent("Copy the address and open it in your browser.");
});
