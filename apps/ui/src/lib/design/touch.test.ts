import { expect, it } from "vitest";
import chip from "./Chip.svelte?raw";
import addDevice from "../../routes/AddDevice.svelte?raw";
import deviceCard from "../../routes/DeviceCard.svelte?raw";
import home from "../../routes/Home.svelte?raw";
import join from "../../routes/Join.svelte?raw";
import outboundEditor from "../../routes/OutboundEditor.svelte?raw";
import placementEditor from "../../routes/PlacementEditor.svelte?raw";
import deploy from "../../routes/Deploy.svelte?raw";
import services from "../../routes/Services.svelte?raw";
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

it("toggle_chip_meets_the_touch_target_on_coarse_pointers", () => {
  const rules = coarseRules(chip);
  const toggle = rules.match(/\.toggle\s*\{([^}]*)\}/);
  expect(toggle, "a .toggle rule under (pointer: coarse)").not.toBeNull();
  const body = toggle?.[1] ?? "";
  // At least 44 px tall: the touch token itself, not a smaller fixed height.
  expect(body).toMatch(/(min-)?height:\s*var\(--touch\)/);
  expect(body).not.toMatch(/(^|[^-])height:\s*\d+px/);
});

it("sheet_close_button_never_shrinks_beside_a_long_title", () => {
  const close = sheet.match(/\n\s*\.close\s*\{([^}]*)\}/);
  expect(close?.[1] ?? "").toMatch(/flex-shrink:\s*0/);
});

// Every control the screens draw themselves (not through Button/Chip/Field) that is smaller
// than 44 px by default must reach the touch target on coarse pointers.
it.each([
  ["AddDevice", ".back", addDevice],
  ["AddDevice", ".segment", addDevice],
  ["Join", ".back", join],
  ["Home", ".url", home],
  ["DeviceCard", ".more", deviceCard],
  ["PlacementEditor", ".segment", placementEditor],
  ["OutboundEditor", ".remove", outboundEditor],
  ["Deploy", ".change", deploy],
  ["Services", ".row", services],
])("%s %s meets the touch target on coarse pointers", (_name, selector, source) => {
  const rule = new RegExp(
    selector.replace(".", "\\.") + String.raw`\s*\{[^}]*(min-)?height:\s*var\(--touch\)`,
  );
  expect(coarseRules(source)).toMatch(rule);
});
