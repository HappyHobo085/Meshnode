import { fireEvent, render, screen } from "@testing-library/svelte";
import { get } from "svelte/store";
import { beforeEach, expect, it, vi } from "vitest";
import { member, running, service } from "./fixtures";

const api = vi.hoisted(() => ({
  platform: vi.fn(),
  status: vi.fn(),
  members: vi.fn(),
  services: vi.fn(),
  deployWasmFile: vi.fn(),
  desktop: { pickFolder: vi.fn(), pickFile: vi.fn(), deployStaticDir: vi.fn() },
  android: { pickTree: vi.fn(), pickWasm: vi.fn(), deployStaticTree: vi.fn() },
}));
vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  api,
}));

import { members, services, status, toasts } from "../lib/stores";
import Deploy from "./Deploy.svelte";

const DESK = member({ id: "aa11", name: "Desk", isSelf: true, tags: ["desktop"] });
const DEFAULT_LIMITS = { memoryMb: 64, requestTimeoutMs: 10_000, maxBodyBytes: 8 * 1024 * 1024 };

beforeEach(() => {
  vi.clearAllMocks();
  api.platform.mockResolvedValue("desktop-windows");
  api.status.mockResolvedValue(running());
  api.members.mockResolvedValue([DESK]);
  api.services.mockResolvedValue([]);
  status.set(running());
  members.set([DESK]);
  services.set([]);
});

function open(onclose = vi.fn()) {
  render(Deploy, { open: true, onclose });
  return onclose;
}

it("deploy_static_shows_progress_and_done", async () => {
  api.desktop.pickFolder.mockResolvedValue("C:\\Sites\\My Blog");
  let report: (n: number) => void = () => {};
  let finish: (s: unknown) => void = () => {};
  api.desktop.deployStaticDir.mockImplementation(
    (_p: string, _n: string, _o: unknown, onProgress: (n: number) => void) => {
      report = onProgress;
      onProgress(0);
      return new Promise((resolve) => (finish = resolve));
    },
  );
  open();

  await fireEvent.click(await screen.findByRole("button", { name: /site folder/i }));
  // The name is offered from the folder, in the daemon's alphabet.
  const name = await screen.findByLabelText("Service name");
  expect(name).toHaveValue("my-blog");
  await fireEvent.click(screen.getByRole("button", { name: "Next" }));

  expect(screen.getByRole("radio", { name: "All hosts" })).toBeChecked();
  await fireEvent.click(screen.getByRole("button", { name: "Deploy my-blog" }));

  expect(api.desktop.deployStaticDir).toHaveBeenCalledWith(
    "C:\\Sites\\My Blog",
    "my-blog",
    { limits: DEFAULT_LIMITS, placement: { kind: "allHosts" }, outbound: [] },
    expect.any(Function),
  );
  const bar = await screen.findByRole("progressbar");
  expect(bar).toBeInTheDocument();
  expect(screen.getByText(/0 B read/)).toBeInTheDocument();

  report(640_000);
  await vi.waitFor(() => expect(screen.getByText(/640 kB read/)).toBeInTheDocument());
  report(1_200_000);
  await vi.waitFor(() => expect(screen.getByText(/1\.2 MB read/)).toBeInTheDocument());

  api.services.mockResolvedValue([service({ name: "my-blog" })]);
  finish(service({ name: "my-blog" }));
  expect(await screen.findByRole("heading", { name: "my-blog is deployed" })).toBeInTheDocument();
  expect(screen.queryByRole("progressbar")).toBeNull();
  expect(screen.getByRole("link", { name: "Open my-blog" })).toHaveAttribute(
    "href",
    "#/services/my-blog",
  );
});

it("deploy_picker_cancel_is_quiet", async () => {
  api.desktop.pickFolder.mockRejectedValue({
    code: "pickerCancelled",
    message: "cancelled",
    hint: null,
    reason: null,
  });
  api.desktop.pickFile.mockResolvedValue(null);
  open();

  await fireEvent.click(await screen.findByRole("button", { name: /site folder/i }));
  await vi.waitFor(() => expect(api.desktop.pickFolder).toHaveBeenCalled());
  await fireEvent.click(screen.getByRole("button", { name: /wasm/i }));
  await vi.waitFor(() => expect(api.desktop.pickFile).toHaveBeenCalledWith("wasm"));
  await new Promise((r) => setTimeout(r, 0));

  expect(screen.queryByRole("alert")).toBeNull();
  expect(screen.queryByLabelText("Service name")).toBeNull();
  // Still on the first step, ready to choose again.
  expect(screen.getByRole("button", { name: /site folder/i })).toBeEnabled();
});

it("deploy_error_names_file", async () => {
  api.desktop.pickFolder.mockResolvedValue("C:/Sites/blog");
  api.desktop.deployStaticDir.mockRejectedValue({
    code: "invalidArgument",
    message: "file too large: C:/Sites/blog/media/big.bin (80 MB, the limit is 64 MB)",
    hint: "Leave large files out of the site folder.",
    reason: null,
  });
  open();
  await fireEvent.click(await screen.findByRole("button", { name: /site folder/i }));
  await fireEvent.click(await screen.findByRole("button", { name: "Next" }));
  await fireEvent.click(screen.getByRole("button", { name: "Deploy blog" }));

  const alert = await screen.findByRole("alert");
  expect(alert).toHaveTextContent("/big.bin");
  expect(alert).toHaveTextContent("Leave large files out of the site folder.");
  // Inline, on the step that can retry it.
  expect(screen.getByRole("button", { name: "Deploy blog" })).toBeEnabled();
});

it("deploy_wasm_on_desktop_uses_the_file_picker", async () => {
  api.desktop.pickFile.mockResolvedValue("D:/build/Hello_World.wasm");
  api.deployWasmFile.mockResolvedValue(service({ name: "hello-world", kind: "wasm" }));
  open();
  await fireEvent.click(await screen.findByRole("button", { name: /wasm/i }));
  expect(await screen.findByLabelText("Service name")).toHaveValue("hello-world");
  await fireEvent.click(screen.getByRole("button", { name: "Next" }));
  await fireEvent.click(screen.getByRole("radio", { name: "Hosts with tags" }));
  await fireEvent.click(screen.getByRole("button", { name: "desktop" }));
  await fireEvent.click(screen.getByRole("button", { name: "Deploy hello-world" }));
  expect(api.deployWasmFile).toHaveBeenCalledWith("D:/build/Hello_World.wasm", "hello-world", {
    limits: DEFAULT_LIMITS,
    placement: { kind: "tags", tags: ["desktop"] },
    outbound: [],
  });
  expect(
    await screen.findByRole("heading", { name: "hello-world is deployed" }),
  ).toBeInTheDocument();
});

it("deploy_on_android_uses_the_tree_and_wasm_pickers", async () => {
  api.platform.mockResolvedValue("android");
  api.android.pickTree.mockResolvedValue(
    "content://com.android.externalstorage.documents/tree/primary%3ASites%2Fphotos",
  );
  api.android.deployStaticTree.mockResolvedValue(service({ name: "photos" }));
  api.android.pickWasm.mockResolvedValue("/data/user/0/app/cache/picked/tool.wasm");
  open();

  await fireEvent.click(await screen.findByRole("button", { name: /site folder/i }));
  expect(await screen.findByLabelText("Service name")).toHaveValue("photos");
  await fireEvent.click(screen.getByRole("button", { name: "Next" }));
  await fireEvent.click(screen.getByRole("button", { name: "Deploy photos" }));
  expect(api.android.deployStaticTree).toHaveBeenCalledWith(
    "content://com.android.externalstorage.documents/tree/primary%3ASites%2Fphotos",
    "photos",
    { limits: DEFAULT_LIMITS, placement: { kind: "allHosts" }, outbound: [] },
    expect.any(Function),
  );
  expect(api.desktop.pickFolder).not.toHaveBeenCalled();
  expect(await screen.findByRole("heading", { name: "photos is deployed" })).toBeInTheDocument();

  await fireEvent.click(screen.getByRole("button", { name: "Deploy another" }));
  await fireEvent.click(await screen.findByRole("button", { name: /wasm/i }));
  expect(await screen.findByLabelText("Service name")).toHaveValue("tool");
  expect(api.android.pickWasm).toHaveBeenCalled();
  expect(api.desktop.pickFile).not.toHaveBeenCalled();
});

it("deploy_name_follows_the_daemon_rules", async () => {
  api.desktop.pickFolder.mockResolvedValue("C:/Sites/blog");
  services.set([service({ name: "blog", version: 3 })]);
  open();
  await fireEvent.click(await screen.findByRole("button", { name: /site folder/i }));
  const name = await screen.findByLabelText("Service name");
  // An existing name is allowed: the deploy is its next version.
  expect(screen.getByText(/replaces blog v3/i)).toBeInTheDocument();
  await fireEvent.input(name, { target: { value: "My Blog!" } });
  await fireEvent.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByRole("alert")).toHaveTextContent(
    "Use 1 to 32 lowercase letters, digits and dashes",
  );
  expect(screen.queryByRole("radio", { name: "All hosts" })).toBeNull();
});

it("deploy_closed_while_running_ends_with_a_toast_not_a_stale_screen", async () => {
  api.desktop.pickFolder.mockResolvedValue("C:/Sites/blog");
  let finish: (s: unknown) => void = () => {};
  api.desktop.deployStaticDir.mockReturnValue(new Promise((resolve) => (finish = resolve)));
  const view = render(Deploy, { open: true, onclose: vi.fn() });
  await fireEvent.click(await screen.findByRole("button", { name: /site folder/i }));
  await fireEvent.click(await screen.findByRole("button", { name: "Next" }));
  await fireEvent.click(screen.getByRole("button", { name: "Deploy blog" }));
  expect(await screen.findByRole("progressbar")).toBeInTheDocument();

  // Closed and opened again: a fresh start, which the old deploy must not take over.
  await view.rerender({ open: false, onclose: vi.fn() });
  await view.rerender({ open: true, onclose: vi.fn() });
  expect(await screen.findByRole("button", { name: /site folder/i })).toBeInTheDocument();
  finish(service({ name: "blog" }));
  await vi.waitFor(() => expect(get(toasts).map((t) => t.message)).toContain("Deployed blog"));
  expect(screen.queryByRole("heading", { name: "blog is deployed" })).toBeNull();
  expect(screen.getByRole("button", { name: /site folder/i })).toBeInTheDocument();
});
