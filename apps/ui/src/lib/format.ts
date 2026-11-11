const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** "just now", "2 min ago", "3 h ago", "1 day ago", "2 days ago". */
export function relativeTime(ms: number, now: number = Date.now()): string {
  const ago = now - ms;
  if (ago < MINUTE) return "just now";
  if (ago < HOUR) return `${Math.floor(ago / MINUTE)} min ago`;
  if (ago < DAY) return `${Math.floor(ago / HOUR)} h ago`;
  const days = Math.floor(ago / DAY);
  return days === 1 ? "1 day ago" : `${days} days ago`;
}

const UNITS = ["kB", "MB", "GB", "TB"] as const;

/** Decimal (SI) sizes: "999 B", "1.2 MB", "45 MB". One decimal below 10. */
export function bytes(n: number): string {
  if (n < 1000) return `${Math.round(n)} B`;
  let value = n;
  let unit: string = "B";
  for (const u of UNITS) {
    if (value < 1000) break;
    value /= 1000;
    unit = u;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${unit}`;
}

/** Round-trip time: "—" when unknown, "<1 ms", "3.4 ms", "43 ms", "1.3 s". */
export function rtt(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return "—";
  if (ms < 1) return "<1 ms";
  if (ms < 10) return `${ms.toFixed(1)} ms`;
  if (ms < 1000) return `${Math.round(ms)} ms`;
  return `${(ms / 1000).toFixed(1)} s`;
}
