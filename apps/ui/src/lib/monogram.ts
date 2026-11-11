const segmenter = new Intl.Segmenter(undefined, { granularity: "grapheme" });

function graphemes(word: string): string[] {
  return Array.from(segmenter.segment(word), (s) => s.segment);
}

/**
 * Two letters for a device's monogram: the first grapheme of the first two words, or
 * the first two graphemes of a single word, upper-cased. Graphemes (not code units)
 * keep emoji such as "👩‍💻" whole. An empty name gives "?".
 */
export function monogram(name: string): string {
  const words = name.trim().split(/\s+/).filter(Boolean);
  const [first, second] = words;
  if (first === undefined) return "?";
  const letters =
    second === undefined
      ? graphemes(first).slice(0, 2)
      : [graphemes(first)[0] ?? "", graphemes(second)[0] ?? ""];
  return letters.join("").toLocaleUpperCase();
}
