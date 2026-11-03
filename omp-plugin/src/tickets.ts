// Ticket references in the agent's replies: `#12` (a ticket of the repository at hand) and
// `#weida-12` are drawn as links of the scheme `zeughaus-ticket:`, and picking one asks the
// editor that drives this zeughaus terminal to show the ticket: the terminal user variable
// `zeughaus_show` (iTerm2's `OSC 1337;SetUserVar`, base64 of a JSON object), which the runner
// passes on as an ordered terminal event. zeughaus-link's `ShowTicket` is the reading side.

export const LINK_SCHEME = "zeughaus-ticket";
export const SHOW_VAR = "zeughaus_show";

const MAX_REPO = 100;

export interface TicketRef {
  repo: string;
  number: number;
}

/** The ticket a `zeughaus-ticket:<repo>/<n>` link names; the scheme is case-insensitive. */
export function parseTicketLink(url: string): TicketRef | undefined {
  const colon = url.indexOf(":");
  if (colon === -1 || url.slice(0, colon).toLowerCase() !== LINK_SCHEME) return undefined;
  const m = /^([a-z0-9][a-z0-9-]*)\/([1-9][0-9]*)$/.exec(url.slice(colon + 1));
  if (!m || (m[1] as string).length > MAX_REPO) return undefined;
  return { repo: m[1] as string, number: Number(m[2]) };
}

/** The escape sequence that sets `zeughaus_show` to a request for `t`, made at `now` (ms). */
export function showSequence(t: TicketRef, now: number): string {
  const json = JSON.stringify({ v: 1, repo: t.repo, number: t.number, at: now });
  return `\x1b]1337;SetUserVar=${SHOW_VAR}=${Buffer.from(json).toString("base64")}\x07`;
}

// What is left as it is in a line: code spans, markdown links and autolinks.
const KEEP = /(`+)[\s\S]*?\1|\[[^\]\n]*\]\([^)\n]*\)|<[a-z][a-z0-9+.-]*:[^>\s]*>/gi;
// `#12` and `#weida-12`, not inside a word, an anchor (`/#12`), an entity or `##2`.
const REF = /(?<![\w#&/\\-])#(?:([a-z0-9][a-z0-9-]*)-)?([0-9]+)(?![\w-])/g;

/**
 * `text` with its ticket references as markdown links of {@link LINK_SCHEME}: `#weida-12` names
 * ticket 12 of weida, `#12` ticket 12 of `own`, and stays plain text without one. Fenced code,
 * code spans and existing links are left alone. `undefined` when nothing changed.
 */
export function linkTickets(text: string, own: string | undefined): string | undefined {
  const link = (whole: string, repo: string | undefined, n: string): string => {
    if (Number(n) === 0) return whole;
    if (repo !== undefined) {
      return repo.length > MAX_REPO ? whole : `[#${repo}-${n}](${LINK_SCHEME}:${repo}/${n})`;
    }
    return own === undefined ? whole : `[#${n}](${LINK_SCHEME}:${own}/${n})`;
  };
  let fence: string | undefined; // the three characters that close the open fence
  let changed = false;
  const lines = text.split(/(?<=\n)/).map((line) => {
    const start = line.trimStart().slice(0, 3);
    if (fence !== undefined) {
      if (start === fence) fence = undefined;
      return line;
    }
    if (start === "```" || start === "~~~") {
      fence = start;
      return line;
    }
    let out = "";
    let from = 0;
    for (const kept of line.matchAll(KEEP)) {
      out += line.slice(from, kept.index).replace(REF, link) + kept[0];
      from = kept.index + kept[0].length;
    }
    out += line.slice(from).replace(REF, link);
    if (out !== line) changed = true;
    return out;
  });
  return changed ? lines.join("") : undefined;
}
