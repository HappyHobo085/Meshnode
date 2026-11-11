/** The geometry of a QR code: everything needed to draw it, and nothing else. */
export interface QrShape {
  viewBox: string;
  d: string;
}

const VIEWBOX = /^[\d.\s-]+$/;
const PATH_DATA = /^[MmHhVvLlZz\d.,\s-]+$/;

/**
 * Reads the invite's QR (`InviteDto.qrSvg`) as data, never as markup: only the root
 * `viewBox` and the `d` of each `<path>` are kept, and both must look like plain geometry.
 * The caller draws a fresh `<svg>` from them, so nothing else from the string (attributes,
 * scripts, styles, links) can reach the page. `null` when the string is not a usable QR.
 */
export function parseQr(svg: string): QrShape | null {
  if (!svg) return null;
  const doc = new DOMParser().parseFromString(svg, "image/svg+xml");
  const root = doc.documentElement;
  if (root.localName !== "svg" || doc.getElementsByTagName("parsererror").length > 0) return null;
  const viewBox = root.getAttribute("viewBox")?.trim() ?? "";
  if (!VIEWBOX.test(viewBox)) return null;
  const d = Array.from(root.getElementsByTagName("path"), (p) => p.getAttribute("d")?.trim() ?? "")
    .filter((s) => PATH_DATA.test(s))
    .join("");
  return d ? { viewBox, d } : null;
}
