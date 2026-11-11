import { expect, it } from "vitest";
import { percentile, summarize } from "./probe";

it("percentile_nearest_rank_cases", () => {
  const tens = Array.from({ length: 20 }, (_, i) => (i + 1) * 10); // 10..200
  expect(percentile(tens, 50)).toBe(100);
  expect(percentile(tens, 95)).toBe(190);
  expect(percentile(tens, 100)).toBe(200);
  expect(percentile(tens, 0)).toBe(10);
  // Order of the input doesn't matter.
  expect(percentile([...tens].reverse(), 50)).toBe(100);
  expect(percentile([7], 95)).toBe(7);
  expect(percentile([], 50)).toBeNull();
  expect(percentile([1, 2, 3, 4], 50)).toBe(2);
  expect(percentile([1, 2, 3, 4], 75)).toBe(3);
});

it("summarize_counts_failures_and_ignores_them_in_the_percentiles", () => {
  const s = summarize([
    { seq: 1, ms: 30, connectMs: null, error: null },
    { seq: 2, ms: null, connectMs: null, error: "timed out" },
    { seq: 3, ms: 10, connectMs: 4, error: null },
    { seq: 4, ms: 20, connectMs: null, error: null },
  ]);
  expect(s).toEqual({ p50: 20, p95: 30, max: 30, ok: 3, failed: 1 });
  expect(summarize([])).toEqual({ p50: null, p95: null, max: null, ok: 0, failed: 0 });
});
