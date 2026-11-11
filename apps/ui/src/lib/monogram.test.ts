import { expect, it } from "vitest";
import { monogram } from "./monogram";

it("monogram_handles_emoji_and_short_names", () => {
  expect(monogram("Pixel 8")).toBe("P8");
  expect(monogram("desk")).toBe("DE");
  expect(monogram("👩‍💻 laptop")).toBe("👩‍💻L");
  expect(monogram("x")).toBe("X");
  expect(monogram("")).toBe("?");
});

it("monogram_ignores_surrounding_space_and_extra_words", () => {
  expect(monogram("  living room tv ")).toBe("LR");
  expect(monogram("   ")).toBe("?");
  expect(monogram("éclair")).toBe("ÉC");
});
