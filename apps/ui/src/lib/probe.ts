import type { ProbeSampleDto } from "./api";

/** The latency probe Settings runs: 20 samples, one a second, each allowed 30 s. */
export const PROBE = { count: 20, intervalMs: 1000, timeoutMs: 30_000 } as const;

/**
 * The `p`th percentile (0–100) by the nearest-rank method: the smallest value with at least
 * p % of the values at or below it. `null` for no values.
 */
export function percentile(values: readonly number[], p: number): number | null {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const rank = Math.max(1, Math.ceil((Math.min(100, Math.max(0, p)) / 100) * sorted.length));
  return sorted[rank - 1]!;
}

export interface ProbeSummary {
  p50: number | null;
  p95: number | null;
  max: number | null;
  /** Samples that answered. */
  ok: number;
  /** Samples that failed (an error, or no time). */
  failed: number;
}

/** p50, p95 and max over the samples that answered, and how many failed. */
export function summarize(samples: readonly ProbeSampleDto[]): ProbeSummary {
  const ms = samples.filter((s) => s.error === null && s.ms !== null).map((s) => s.ms as number);
  return {
    p50: percentile(ms, 50),
    p95: percentile(ms, 95),
    max: ms.length ? Math.max(...ms) : null,
    ok: ms.length,
    failed: samples.length - ms.length,
  };
}

/** A round ceiling for the chart's y axis: 1, 2 or 5 × 10ⁿ, at least 10 ms. */
export function niceCeiling(max: number): number {
  const v = Math.max(10, max);
  const pow = 10 ** Math.floor(Math.log10(v));
  for (const m of [1, 2, 5, 10]) if (m * pow >= v) return m * pow;
  return 10 * pow;
}
