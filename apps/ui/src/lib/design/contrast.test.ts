import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

// WCAG AA for the text colours the screens use, computed from tokens.css itself, so a
// token change that drops a pair below 4.5:1 fails here. (Vitest stubs `.css` imports.)
const css = readFileSync(`${process.cwd()}/src/lib/design/tokens.css`, "utf8");

type Rgba = [number, number, number, number];

/** Each `--name: light-dark(a, b)` token as its [light, dark] values. */
const TOKENS: Record<string, [string, string]> = {};
for (const m of css.matchAll(/--([\w-]+):\s*light-dark\(([^;]+)\);/g)) {
  const parts = m[2]!.split(/,(?![^(]*\))/).map((s) => s.trim());
  TOKENS[m[1]!] = [parts[0]!, parts[1]!];
}

function hslToRgb(h: number, s: number, l: number): [number, number, number] {
  const a = s * Math.min(l, 1 - l);
  const f = (n: number) => {
    const k = (n + h / 30) % 12;
    return 255 * (l - a * Math.max(-1, Math.min(k - 3, 9 - k, 1)));
  };
  return [f(0), f(8), f(4)];
}

/** Parses #rrggbb, `rgb(r g b / a)` and `hsl(var(--h) s% l% / a)` (with `hue` for --h). */
function parse(c: string, hue = 0): Rgba {
  if (c.startsWith("#")) {
    return [1, 3, 5].map((i) => parseInt(c.slice(i, i + 2), 16)).concat(1) as Rgba;
  }
  const rgb = /rgb\((\d+) (\d+) (\d+)(?: \/ ([\d.]+))?\)/.exec(c);
  if (rgb) return [+rgb[1]!, +rgb[2]!, +rgb[3]!, rgb[4] ? +rgb[4] : 1];
  const hsl = /hsl\(var\(--h\) ([\d.]+)% ([\d.]+)%(?: \/ ([\d.]+))?\)/.exec(c);
  if (hsl) return [...hslToRgb(hue, +hsl[1]! / 100, +hsl[2]! / 100), hsl[3] ? +hsl[3] : 1];
  throw new Error(`unparsed colour ${c}`);
}

/** `fg` (possibly translucent) composited over an opaque `bg`. */
function over(fg: Rgba, bg: Rgba): Rgba {
  const a = fg[3];
  return [0, 1, 2].map((i) => fg[i]! * a + bg[i]! * (1 - a)).concat(1) as Rgba;
}

function luminance([r, g, b]: Rgba): number {
  const lin = (v: number) => {
    const s = v / 255;
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

function ratio(a: Rgba, b: Rgba): number {
  const [x, y] = [luminance(a), luminance(b)].sort((p, q) => q - p) as [number, number];
  return (x + 0.05) / (y + 0.05);
}

const THEMES = [
  ["light", 0],
  ["dark", 1],
] as const;

/** Text tokens used for body, label or status text in the screens. */
const TEXT = ["ink", "ink-2", "ink-3", "signal", "ok", "warn", "danger"];
/**
 * Backgrounds they sit on: the page, a card (a muted card keeps --surface; only its edge
 * dims), and the inset fields (gateway URL, ticket box, port pills, segments).
 */
const PLAIN_BACKGROUNDS = ["bg", "surface", "surface-2"];
/** Tinted panels: [text, translucent background] composited over --surface. */
const TINTED: [string, string][] = [
  ["signal", "signal-soft"], // "This device" pill
  ["ink", "signal-soft"], // selected role tile title
  ["ink-2", "signal-soft"], // selected role tile description
  ["danger", "danger-soft"], // Join error
  ["ink-2", "danger-soft"], // Join error hint
];

describe.each(THEMES)("%s theme", (_name, i) => {
  const color = (t: string, hue = 0) => parse(TOKENS[t]![i], hue);

  it("text_tokens_meet_aa_on_every_background", () => {
    for (const t of TEXT) {
      for (const b of PLAIN_BACKGROUNDS) {
        expect(ratio(color(t), color(b)), `${t} on ${b}`).toBeGreaterThanOrEqual(4.5);
      }
    }
  });

  it("text_meets_aa_on_tinted_panels", () => {
    const surface = color("surface");
    for (const [t, b] of TINTED) {
      expect(ratio(color(t), over(color(b), surface)), `${t} on ${b}`).toBeGreaterThanOrEqual(4.5);
    }
  });

  it("button_text_meets_aa", () => {
    expect(ratio(color("signal-ink"), color("signal"))).toBeGreaterThanOrEqual(4.5);
    expect(ratio(color("danger-ink"), color("danger"))).toBeGreaterThanOrEqual(4.5);
  });

  it("device_ink_meets_aa_on_its_chip_at_every_hue", () => {
    for (let h = 0; h < 360; h++) {
      const chip = over(color("accent-soft", h), color("surface"));
      expect(ratio(color("accent-ink", h), chip), `hue ${h}`).toBeGreaterThanOrEqual(4.5);
    }
  });
});
