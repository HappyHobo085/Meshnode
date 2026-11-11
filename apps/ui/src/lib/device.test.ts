import { expect, it } from "vitest";
import { defaultDeviceName, platformOf, refusalText } from "./device";

it("default_device_name_per_platform", () => {
  const s23 =
    "Mozilla/5.0 (Linux; Android 14; SM-S911B Build/UP1A.231005.007; wv) AppleWebKit/537.36";
  expect(defaultDeviceName("android", s23)).toBe("SM-S911B");
  expect(defaultDeviceName("android", "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit")).toBe(
    "Android phone",
  );
  // No model at all in the user agent.
  expect(defaultDeviceName("android", "Mozilla/5.0 (Linux; Android 14; wv)")).toBe("Android phone");
  expect(defaultDeviceName("desktop-windows", "")).toBe("Windows PC");
  expect(defaultDeviceName("desktop-macos", "")).toBe("Mac");
  expect(defaultDeviceName("desktop-linux", "")).toBe("Linux PC");
  expect(defaultDeviceName(null, "")).toBe("");
});

it("platform_icons", () => {
  expect(platformOf("android").icon).toBe("phone");
  expect(platformOf("windows").label).toBe("Windows");
  expect(platformOf("linux").icon).toBe("server");
  expect(platformOf("").label).toBe("Unknown");
});

it("every_refusal_has_words", () => {
  for (const r of [
    "expired",
    "used",
    "unknown",
    "wrongMesh",
    "removed",
    "inviterUnavailable",
  ] as const) {
    expect(refusalText(r)).toBeTruthy();
  }
  expect(refusalText(null)).toBeNull();
});
