import { describe, expect, it } from "vitest";
import { hrefOf, parseHash } from "./router";

describe("router", () => {
  it("parses_every_route", () => {
    expect(parseHash("")).toEqual({ name: "home" });
    expect(parseHash("#/")).toEqual({ name: "home" });
    expect(parseHash("#/add")).toEqual({ name: "add" });
    expect(parseHash("#/join")).toEqual({ name: "join" });
    expect(parseHash("#/services")).toEqual({ name: "services" });
    expect(parseHash("#/services/blog%20site")).toEqual({
      name: "service",
      params: { name: "blog site" },
    });
    expect(parseHash("#/deploy")).toEqual({ name: "deploy" });
    expect(parseHash("#/settings")).toEqual({ name: "settings" });
    expect(parseHash("#/logs")).toEqual({ name: "logs" });
    expect(parseHash("#/first-run")).toEqual({ name: "first-run" });
    expect(parseHash("#/no-daemon")).toEqual({ name: "no-daemon" });
    expect(parseHash("#/stopped")).toEqual({ name: "stopped" });
  });

  it("unknown_routes_go_home", () => {
    expect(parseHash("#/nope")).toEqual({ name: "home" });
    expect(parseHash("#/services/a/b")).toEqual({ name: "home" });
    expect(parseHash("#/services/%E0%A4%A")).toEqual({ name: "home" });
  });

  it("href_round_trips", () => {
    expect(hrefOf({ name: "service", params: { name: "blog site" } })).toBe(
      "#/services/blog%20site",
    );
    expect(hrefOf({ name: "home" })).toBe("#/");
    expect(parseHash(hrefOf({ name: "service", params: { name: "a/b" } }))).toEqual({
      name: "service",
      params: { name: "a/b" },
    });
  });
});
