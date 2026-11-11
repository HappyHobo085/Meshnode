import { expect, it } from "vitest";
import { levelOf, splitMatches } from "./logs";

it("level_is_parsed_from_the_lines_level_token", () => {
  expect(levelOf("2026-11-11T10:00:00.123Z  INFO mesh_host: started")).toBe("info");
  expect(levelOf("2026-11-11T10:00:00.123Z ERROR mesh_net: refused")).toBe("error");
  expect(levelOf("2026-11-11T10:00:00Z  WARN x: y")).toBe("warn");
  expect(levelOf("2026-11-11T10:00:00Z DEBUG x: y")).toBe("debug");
  expect(levelOf("2026-11-11T10:00:00Z TRACE x: y")).toBe("trace");
  expect(levelOf("level=warn msg=hello")).toBe("warn");
  // A message that merely mentions a level later on is not that level.
  expect(levelOf("2026-11-11T10:00:00Z  INFO x: the ERROR count is 0")).toBe("info");
  // Continuation lines (a backtrace) have no level of their own.
  expect(levelOf("   at mesh_host::run (src/run.rs:10)")).toBeNull();
});

it("search_matches_are_split_out_case_insensitively", () => {
  expect(splitMatches("Peer joined peer", "peer")).toEqual([
    { text: "Peer", hit: true },
    { text: " joined ", hit: false },
    { text: "peer", hit: true },
  ]);
  expect(splitMatches("abc", "")).toEqual([{ text: "abc", hit: false }]);
  expect(splitMatches("a.c abc", ".")).toEqual([
    { text: "a", hit: false },
    { text: ".", hit: true },
    { text: "c abc", hit: false },
  ]);
});
