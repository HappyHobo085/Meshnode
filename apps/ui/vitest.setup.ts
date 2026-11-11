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
