import { readable } from "svelte/store";

/** The app's screens, addressed by the URL hash (`#/services/blog`). */
export type Route =
  | { name: "home" }
  | { name: "add" }
  | { name: "join" }
  | { name: "services" }
  | { name: "service"; params: { name: string } }
  | { name: "deploy" }
  | { name: "settings" }
  | { name: "logs" }
  | { name: "first-run" }
  | { name: "no-daemon" }
  | { name: "stopped" };

type SimpleRoute = Exclude<Route, { name: "service" }>["name"];

const SIMPLE: Record<string, SimpleRoute> = {
  "": "home",
  add: "add",
  join: "join",
  services: "services",
  deploy: "deploy",
  settings: "settings",
  logs: "logs",
  "first-run": "first-run",
  "no-daemon": "no-daemon",
  stopped: "stopped",
};

/** Parse a location hash; anything unknown or malformed is Home. */
export function parseHash(hash: string): Route {
  const path = hash.replace(/^#\/?/, "").replace(/\/$/, "");
  const parts = path.split("/");
  const [head, tail, ...rest] = parts;
  if (parts.length === 1) {
    const name = SIMPLE[head ?? ""];
    return name ? { name } : { name: "home" };
  }
  if (head === "services" && tail && rest.length === 0) {
    try {
      return { name: "service", params: { name: decodeURIComponent(tail) } };
    } catch {
      return { name: "home" };
    }
  }
  return { name: "home" };
}

/** The hash for a route, suitable for `href` or `navigate`. */
export function hrefOf(route: Route): string {
  switch (route.name) {
    case "home":
      return "#/";
    case "service":
      return `#/services/${encodeURIComponent(route.params.name)}`;
    default:
      return `#/${route.name}`;
  }
}

/** The current route, following `hashchange`. */
export const route = readable<Route>(parseHash(globalThis.location?.hash ?? ""), (set) => {
  const update = () => set(parseHash(location.hash));
  update();
  addEventListener("hashchange", update);
  return () => removeEventListener("hashchange", update);
});

/** Go to a route. `replace` swaps the history entry (for redirects such as first run). */
export function navigate(to: Route, { replace = false }: { replace?: boolean } = {}): void {
  const href = hrefOf(to);
  if (location.hash === href) return;
  if (replace) {
    history.replaceState(null, "", href);
    dispatchEvent(new HashChangeEvent("hashchange"));
  } else {
    location.hash = href;
  }
}
