import { expect, it } from "vitest";
import sheet from "./Sheet.svelte?raw";

/** The rules inside `@media (pointer: coarse) { … }` blocks of a component's <style>. */
function coarseRules(source: string): string {
  const blocks: string[] = [];
  const re = /@media \(pointer: coarse\) \{/g;
  for (let m = re.exec(source); m; m = re.exec(source)) {
    let depth = 1;
    let i = re.lastIndex;
    for (; i < source.length && depth > 0; i++) {
      if (source[i] === "{") depth++;
      else if (source[i] === "}") depth--;
    }
    blocks.push(source.slice(re.lastIndex, i - 1));
  }
  return blocks.join("\n");
}

it("sheet_close_button_meets_the_touch_target_on_coarse_pointers", () => {
  const rules = coarseRules(sheet);
  expect(rules).toMatch(/\.close\s*\{[^}]*width:\s*var\(--touch\)/);
  expect(rules).toMatch(/\.close\s*\{[^}]*height:\s*var\(--touch\)/);
});
