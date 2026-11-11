/** Log levels, quietest first. */
export const LEVELS = ["trace", "debug", "info", "warn", "error"] as const;
export type Level = (typeof LEVELS)[number];

/** The view keeps this many lines; older ones are dropped. */
export const LOG_CAPACITY = 10_000;
/** Lines of history asked for when the view opens (or resumes). */
export const LOG_HISTORY = 1000;

// tracing's format: "<timestamp> <LEVEL> <target>: <message>", with the level padded to
// five characters; or logfmt's "level=info". Only the head of the line is looked at, so a
// message that mentions "ERROR" later on keeps its own level.
const TOKEN = /^\S*\s+(TRACE|DEBUG|INFO|WARN|ERROR)\b|^(TRACE|DEBUG|INFO|WARN|ERROR)\b/;
const LOGFMT = /(?:^|\s)level=(trace|debug|info|warn|error)\b/i;

/** The line's level token, or `null` for a line without one (a continuation line). */
export function levelOf(line: string): Level | null {
  const m = TOKEN.exec(line);
  if (m) return (m[1] ?? m[2])!.toLowerCase() as Level;
  const f = LOGFMT.exec(line.slice(0, 80));
  return f ? (f[1]!.toLowerCase() as Level) : null;
}

export interface Piece {
  text: string;
  hit: boolean;
}

/** `text` split around case-insensitive matches of `query`, for highlighting. */
export function splitMatches(text: string, query: string): Piece[] {
  if (!query) return [{ text, hit: false }];
  const hay = text.toLowerCase();
  const needle = query.toLowerCase();
  const out: Piece[] = [];
  let from = 0;
  for (let at = hay.indexOf(needle); at !== -1; at = hay.indexOf(needle, from)) {
    if (at > from) out.push({ text: text.slice(from, at), hit: false });
    out.push({ text: text.slice(at, at + needle.length), hit: true });
    from = at + needle.length;
  }
  if (from < text.length) out.push({ text: text.slice(from), hit: false });
  return out;
}
