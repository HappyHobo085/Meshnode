import { describe, expect, it } from "vitest";
import { bytes, clock, gatewayOpenUrl, relativeTime, rtt } from "./format";

const now = 1_700_000_000_000;
const s = 1000;
const min = 60 * s;
const h = 60 * min;
const day = 24 * h;

describe("format", () => {
  it("relative_time_cases", () => {
    expect(relativeTime(now, now)).toBe("just now");
    expect(relativeTime(now - 30 * s, now)).toBe("just now");
    expect(relativeTime(now + 5 * s, now)).toBe("just now");
    expect(relativeTime(now - 2 * min, now)).toBe("2 min ago");
    expect(relativeTime(now - 59 * min, now)).toBe("59 min ago");
    expect(relativeTime(now - 3 * h, now)).toBe("3 h ago");
    expect(relativeTime(now - 1 * day, now)).toBe("1 day ago");
    expect(relativeTime(now - 2 * day - 3 * h, now)).toBe("2 days ago");
  });

  it("bytes_cases", () => {
    expect(bytes(0)).toBe("0 B");
    expect(bytes(999)).toBe("999 B");
    expect(bytes(1_200)).toBe("1.2 kB");
    expect(bytes(1_200_000)).toBe("1.2 MB");
    expect(bytes(45_300_000)).toBe("45 MB");
    expect(bytes(3_500_000_000)).toBe("3.5 GB");
  });

  it("rtt_cases", () => {
    expect(rtt(null)).toBe("—");
    expect(rtt(0.4)).toBe("<1 ms");
    expect(rtt(3.42)).toBe("3.4 ms");
    expect(rtt(42.6)).toBe("43 ms");
    expect(rtt(1_250)).toBe("1.3 s");
  });
});

it("clock_cases", () => {
  expect(clock(900_000)).toBe("15:00");
  expect(clock(899_000)).toBe("14:59");
  expect(clock(3_600_000)).toBe("60:00");
  expect(clock(1)).toBe("0:01");
  expect(clock(0)).toBe("0:00");
  expect(clock(-5_000)).toBe("0:00");
});

describe("gatewayOpenUrl", () => {
  it("opens a wildcard listen address on this machine", () => {
    expect(gatewayOpenUrl("http://0.0.0.0:7780")).toBe("http://127.0.0.1:7780");
    expect(gatewayOpenUrl("http://[::]:7780")).toBe("http://127.0.0.1:7780");
  });
  it("leaves other addresses as they are", () => {
    expect(gatewayOpenUrl("http://127.0.0.1:7780")).toBe("http://127.0.0.1:7780");
    expect(gatewayOpenUrl("http://192.168.1.5:7780")).toBe("http://192.168.1.5:7780");
    // Loopback over IPv6 is not 127.0.0.1: left for open_url to refuse (shown inline).
    expect(gatewayOpenUrl("http://[::1]:7780")).toBe("http://[::1]:7780");
  });
});
