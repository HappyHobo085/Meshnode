import { writable } from "svelte/store";
import type { IconName } from "./design/Icon.svelte";
import type { Platform } from "./api";
import type { JoinRefusalDto } from "./generated/JoinRefusalDto";

/** The name typed on First run, carried over to Join so it isn't asked twice. */
export const chosenName = writable<string | null>(null);

/**
 * A starting name for this device. The webview can't read the host name, so on desktop it is
 * the kind of computer; on Android it is the phone model when the user agent still carries one
 * (reduced user agents send "K" instead).
 */
export function defaultDeviceName(
  platform: Platform | null,
  ua: string = globalThis.navigator?.userAgent ?? "",
): string {
  switch (platform) {
    case "android": {
      const model = /Android [\d.]+; ([^;)]+?)(?: Build\/[^;)]*)?[;)]/.exec(ua)?.[1]?.trim();
      return model && model !== "K" ? model : "My phone";
    }
    case "desktop-windows":
      return "Windows PC";
    case "desktop-macos":
      return "Mac";
    case "desktop-linux":
      return "Linux PC";
    default:
      return "";
  }
}

/** A member's platform (`std::env::consts::OS` from Rust) as an icon and a word. */
export function platformOf(platform: string): { icon: IconName; label: string } {
  const p = platform.toLowerCase();
  if (p.includes("android") || p.includes("ios")) return { icon: "phone", label: "Phone" };
  if (p.includes("windows")) return { icon: "desktop", label: "Windows" };
  if (p.includes("mac")) return { icon: "laptop", label: "Mac" };
  if (p.includes("linux")) return { icon: "server", label: "Linux" };
  return { icon: "desktop", label: platform || "Unknown" };
}

/** Why an invite was refused, in the user's words. */
export function refusalText(reason: JoinRefusalDto | null): string | null {
  switch (reason) {
    case "expired":
      return "This invite has expired";
    case "used":
      return "This invite was already used";
    case "unknown":
      return "This invite isn't known to that mesh";
    case "wrongMesh":
      return "This invite is for a different mesh";
    case "removed":
      return "This device was removed from that mesh";
    case "inviterUnavailable":
      return "The device that made this invite can't be reached";
    default:
      return null;
  }
}

/** Copies text to the clipboard; resolves `false` when the clipboard is unavailable. */
export async function copyText(text: string): Promise<boolean> {
  try {
    await globalThis.navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}
