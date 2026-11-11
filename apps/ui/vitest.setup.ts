import "@testing-library/jest-dom/vitest";

// jsdom has no Web Animations API, which Svelte transitions use.
if (typeof Element.prototype.animate !== "function") {
  Element.prototype.animate = function () {
    const animation = {
      finished: Promise.resolve(),
      onfinish: null as null | (() => void),
      cancel() {},
      finish() {},
      play() {},
      pause() {},
      currentTime: 0,
    };
    queueMicrotask(() => animation.onfinish?.());
    return animation as unknown as Animation;
  };
}

// jsdom has no ResizeObserver, which `bind:clientHeight` uses. It never lays out, so a stub
// that observes nothing is faithful: sizes stay 0.
if (typeof globalThis.ResizeObserver !== "function") {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver;
}
