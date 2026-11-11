import { readFileSync } from "node:fs";
import { expect, it } from "vitest";

// Vitest stubs `.css` imports, so the stylesheet is read from disk.
const tokens = readFileSync(`${process.cwd()}/src/lib/design/tokens.css`, "utf8");

it("device_accent_follows_the_hue_set_on_any_element", () => {
  // A custom property computed on :root keeps :root's --h forever; setting --h on a card
  // or monogram would then change nothing. The accent must be computed per element.
  const rule = /\*,\s*::before,\s*::after\s*\{([^}]*)\}/.exec(tokens)?.[1] ?? "";
  for (const name of ["--accent", "--accent-soft", "--accent-ink"]) {
    expect(rule, name).toMatch(new RegExp(String.raw`${name}:[^;]*var\(--h\)`));
  }
  const root = [...tokens.matchAll(/:root\s*\{([^}]*)\}/g)].map((m) => m[1]).join("\n");
  expect(root).not.toMatch(/--accent(-soft|-ink)?:/);
});
