import { get } from "svelte/store";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { StatusDto } from "./generated/StatusDto";
import type { UiError } from "./generated/UiError";

const apiMock = vi.hoisted(() => ({
  status: vi.fn(),
  members: vi.fn(),
  services: vi.fn(),
}));
vi.mock("./api", () => ({ api: apiMock }));

type Stores = typeof import("./stores");

const running: StatusDto = {
  state: "running",
  nodeId: "ab12",
  meshId: "cd34",
  name: "desk",
  role: "host",
  tags: [],
  membersTotal: 1,
  membersOnline: 1,
  services: 0,
  gatewayUrl: null,
  ports: [],
  daemonVersion: "0.1.0",
  dataDir: "C:/data",
};

function uiError(code: UiError["code"]): UiError {
  return { code, message: code, hint: null, reason: null };
}

class FakeDoc extends EventTarget {
  visibilityState: DocumentVisibilityState = "visible";
  setVisibility(v: DocumentVisibilityState) {
    this.visibilityState = v;
    this.dispatchEvent(new Event("visibilitychange"));
  }
}

let stores: Stores;
let stop: (() => void) | undefined;

beforeEach(async () => {
  vi.useFakeTimers();
  vi.resetModules();
  apiMock.status.mockReset().mockResolvedValue(running);
  apiMock.members.mockReset().mockResolvedValue([]);
  apiMock.services.mockReset().mockResolvedValue([]);
  stores = await import("./stores");
});

afterEach(() => {
  stop?.();
  stop = undefined;
  vi.useRealTimers();
});

describe("poller", () => {
  it("poller_pauses_when_hidden", async () => {
    const doc = new FakeDoc();
    doc.visibilityState = "hidden";
    stop = stores.startPoller({ intervalMs: 2000, doc });
    await vi.advanceTimersByTimeAsync(10_000);
    expect(apiMock.status).not.toHaveBeenCalled();

    doc.setVisibility("visible");
    await vi.advanceTimersByTimeAsync(0);
    expect(apiMock.status).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(4_000);
    expect(apiMock.status).toHaveBeenCalledTimes(3);

    doc.setVisibility("hidden");
    await vi.advanceTimersByTimeAsync(10_000);
    expect(apiMock.status).toHaveBeenCalledTimes(3);
  });

  it("poller_skips_overlapping_ticks", async () => {
    let inFlight = 0;
    let maxInFlight = 0;
    apiMock.status.mockImplementation(() => {
      inFlight += 1;
      maxInFlight = Math.max(maxInFlight, inFlight);
      return new Promise<StatusDto>((resolve) =>
        setTimeout(() => {
          inFlight -= 1;
          resolve(running);
        }, 5_000),
      );
    });
    stop = stores.startPoller({ intervalMs: 2000, doc: new FakeDoc() });
    await vi.advanceTimersByTimeAsync(4_999);
    expect(apiMock.status).toHaveBeenCalledTimes(1);
    expect(maxInFlight).toBe(1);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(maxInFlight).toBe(1);
  });

  it("poller_shows_banner_then_recovers", async () => {
    apiMock.status.mockRejectedValueOnce(uiError("daemonUnreachable"));
    stop = stores.startPoller({ intervalMs: 2000, doc: new FakeDoc() });
    await vi.advanceTimersByTimeAsync(0);
    expect(get(stores.banner)?.kind).toBe("unreachable");
    expect(get(stores.banner)?.error?.code).toBe("daemonUnreachable");

    await vi.advanceTimersByTimeAsync(2_000);
    expect(get(stores.banner)).toBeNull();
    expect(get(stores.status)).toEqual(running);
  });

  it("poller_shows_stopped_and_starting_banners", async () => {
    apiMock.members.mockRejectedValueOnce(uiError("notRunning"));
    apiMock.status.mockResolvedValueOnce(running).mockRejectedValueOnce(uiError("daemonStarting"));
    stop = stores.startPoller({ intervalMs: 2000, doc: new FakeDoc() });
    await vi.advanceTimersByTimeAsync(0);
    expect(get(stores.banner)?.kind).toBe("stopped");
    await vi.advanceTimersByTimeAsync(2_000);
    expect(get(stores.banner)?.kind).toBe("starting");
    await vi.advanceTimersByTimeAsync(2_000);
    expect(get(stores.banner)).toBeNull();
  });

  it("refresh_now_after_mutation", async () => {
    stop = stores.startPoller({ intervalMs: 2000, doc: new FakeDoc() });
    await vi.advanceTimersByTimeAsync(0);
    expect(apiMock.status).toHaveBeenCalledTimes(1);

    const done = stores.refreshNow();
    await vi.advanceTimersByTimeAsync(0);
    await done;
    expect(apiMock.status).toHaveBeenCalledTimes(2);
  });

  it("refresh_now_during_a_poll_runs_a_fresh_one_after_it", async () => {
    let release: (() => void) | undefined;
    apiMock.status.mockImplementationOnce(
      () => new Promise<StatusDto>((resolve) => (release = () => resolve(running))),
    );
    stop = stores.startPoller({ intervalMs: 2000, doc: new FakeDoc() });
    await vi.advanceTimersByTimeAsync(0);
    const done = stores.refreshNow();
    expect(apiMock.status).toHaveBeenCalledTimes(1);
    release?.();
    await done;
    expect(apiMock.status).toHaveBeenCalledTimes(2);
  });

  it("stop_ends_polling", async () => {
    const s = stores.startPoller({ intervalMs: 2000, doc: new FakeDoc() });
    await vi.advanceTimersByTimeAsync(0);
    s();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(apiMock.status).toHaveBeenCalledTimes(1);
  });

  it("skips_members_and_services_when_not_running", async () => {
    apiMock.status.mockResolvedValue({ ...running, state: "uninitialised" });
    stop = stores.startPoller({ intervalMs: 2000, doc: new FakeDoc() });
    await vi.advanceTimersByTimeAsync(0);
    expect(apiMock.members).not.toHaveBeenCalled();
    expect(get(stores.status)?.state).toBe("uninitialised");
    expect(get(stores.banner)).toBeNull();
  });
});
