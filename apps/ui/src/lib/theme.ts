import { writable } from "svelte/store";

/** "system" follows `prefers-color-scheme`; the others pin `[data-theme]` on <html>. */
export type Theme = "system" | "light" | "dark";

const KEY = "meshnode.theme";

function load(): Theme {
  try {
    const v = localStorage.getItem(KEY);
    return v === "light" || v === "dark" ? v : "system";
  } catch {
    return "system";
  }
}

export const theme = writable<Theme>(load());

/** Mirrors the theme onto <html> and remembers it. Call once at start-up. */
export function applyTheme(root: HTMLElement = document.documentElement): () => void {
  return theme.subscribe((t) => {
    if (t === "system") delete root.dataset.theme;
    else root.dataset.theme = t;
    try {
      if (t === "system") localStorage.removeItem(KEY);
      else localStorage.setItem(KEY, t);
    } catch {
      // Storage can be unavailable; the choice then lasts for this session only.
    }
  });
}
