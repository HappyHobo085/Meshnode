/** True when the user asked for reduced motion; Svelte transitions then take 0 ms. */
export function reducedMotion(): boolean {
  return globalThis.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;
}

/** A transition duration (≤ 200 ms by design), or 0 under reduced motion. */
export function dur(ms: 120 | 160 | 200): number {
  return reducedMotion() ? 0 : ms;
}

/** Svelte-transition easing matching `--ease-out` (a soft deceleration). */
export function easeOut(t: number): number {
  return 1 - Math.pow(1 - t, 3);
}
