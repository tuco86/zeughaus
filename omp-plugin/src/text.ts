// The texts zeughaus-runner renders around the CI's answers, ported character for character so
// that the plugin tells an agent what `zeughaus-runner ci` would: the <zeughaus-ci> block
// (zeughaus-runner/src/mcp/repo.rs), a late job's report (tools.rs), a session's description and
// the claim lines (mcp/mod.rs), and the plan header readers (zeughaus-link/src/ci.rs). The shared
// cases in ../fixtures/ hold both sides to the same output. Pure.

import type {
  ClaimSubject,
  ClaimView,
  JobDefView,
  MachineView,
  PipelineView,
  RepoView,
  SessionView,
} from "./protocol.ts";

/** Runs of whitespace, newlines included, as single spaces. */
export function oneLine(text: string): string {
  return text
    .split(/\s+/)
    .filter((part) => part !== "")
    .join(" ");
}

/** The first seven characters of a commit id, `-` when there is none. */
export function short(sha: string | null | undefined): string {
  return sha ? sha.slice(0, 7) : "-";
}

/** Rust's `str::lines`: split at `\n`, a `\r` before it dropped, no empty last line. */
function lines(text: string): string[] {
  const parts = text.split("\n");
  const last = parts.pop() as string;
  const out = parts.map((line) => (line.endsWith("\r") ? line.slice(0, -1) : line));
  if (last !== "") out.push(last);
  return out;
}

/** `items` joined by commas, or `none`. */
function listed(items: string[]): string {
  return items.length === 0 ? "none" : items.join(", ");
}

/** The repository as an agent is told of it, one fact per line and without a final newline. */
export function describe(view: RepoView, head: string | null): string {
  const out = [
    `${view.name} (${view.forge}): ${view.default_branch} ${oneLine(view.branch)}`,
    headLine(view, head),
    `channels: ${listed(
      view.channels.map((channel) =>
        channel.responsible
          ? `${channel.channel} #${channel.latest} ${channel.status} (responsible: ${holderText(channel.responsible)})`
          : `${channel.channel} #${channel.latest} ${channel.status}`,
      ),
    )}`,
    ...jobLines(view),
    runnersLine(view),
    `platforms: ${listed(view.platforms.map(([platform, target]) => `${platform} -> ${target}`))}`,
    ...docLines(view),
  ];
  const setup = view.setup ?? [];
  if (setup.length > 0) out.push("setup:", ...setup.map((line) => `  ${oneLine(line)}`));
  return out.join("\n");
}

/** The open tickets and the tickets that are planned, wait or run, each line left out when it has none. */
function docLines(view: RepoView): string[] {
  const tickets = (view.docs ?? []).filter((doc) => doc.kind === "ticket");
  const open = tickets
    .filter((doc) => doc.state === "open")
    .map((doc) => `#${doc.number} ${doc.title} (from ${doc.source})`);
  const active = tickets
    .filter((doc) => !["open", "done", "dismissed", "merged"].includes(doc.state))
    .map((doc) => `#${doc.number} ${doc.state} ${doc.title}`);
  const out: string[] = [];
  if (open.length > 0) out.push(`Open tickets: ${open.join(", ")}`);
  if (active.length > 0) out.push(`Tickets: ${active.join(", ")}`);
  return out;
}

/** The newest pipeline of the checkout's commit, with why it failed or never ran its jobs. */
function headLine(view: RepoView, head: string | null): string {
  if (head === null) return "HEAD: no commit";
  const sha = short(head);
  const pipeline = view.head;
  if (!pipeline) return `HEAD ${sha}: no pipeline (not pushed, or no job selected)`;
  let line = `HEAD ${sha}: ${pipeline.channel} #${pipeline.number} ${pipeline.status}`;
  if (pipeline.status === "failed") {
    const job = pipeline.jobs.find((j) => j.status === "failed");
    if (job) {
      line += `: ${job.name} failed`;
      const cause = oneLine(job.cause ?? "");
      if (cause !== "") line += `: ${cause}`;
    }
  } else if (pipeline.status !== "running" && pipeline.status !== "succeeded") {
    const note = oneLine(pipeline.note);
    if (note !== "") line += `: ${note}`;
  }
  return line;
}

/** The jobs block, or the one line that says why there is none. Columns count code points. */
function jobLines(view: RepoView): string[] {
  if (view.jobs.length === 0) return [`jobs: ${oneLine(view.jobs_note) || "none"}`];
  const out = [
    view.jobs_sha === null
      ? "jobs (run_jobs takes these names):"
      : `jobs at ${short(view.jobs_sha)} (run_jobs takes these names):`,
  ];
  const places = view.jobs.map(placeOf);
  const nameWidth = Math.max(0, ...view.jobs.map((job) => [...job.name].length));
  // A job that cannot be placed has a sentence for a place, which must not push the others'
  // notes out of line.
  const placeWidth = Math.max(
    0,
    ...view.jobs.flatMap((job, i) => (job.placement === "" ? [[...(places[i] as string)].length] : [])),
  );
  view.jobs.forEach((job, i) => {
    const place = places[i] as string;
    const name = job.name + " ".repeat(Math.max(0, nameWidth - [...job.name].length));
    const column = place + " ".repeat(Math.max(0, placeWidth - [...place].length));
    out.push(`  ${name}  ${column}  ${notesOf(job)}`.trimEnd());
  });
  return out;
}

/** `<runner> <at>`, or why the job has no runner. */
function placeOf(job: JobDefView): string {
  return job.placement === "" ? `${job.runner} ${job.at}` : `cannot be placed: ${oneLine(job.placement)}`;
}

/** What starts the job, its stores, its secrets and what keeps `run_jobs` from it, by semicolons. */
function notesOf(job: JobDefView): string {
  const parts: string[] = [];
  if (job.on.length > 0) parts.push(`on ${job.on.join(", ")}`);
  if (job.needs.length > 0) parts.push(`needs ${job.needs.join(", ")}`);
  if (job.channels.length > 0) parts.push(`channels ${job.channels.join(", ")}`);
  const state = job.state ?? [];
  if (state.length > 0) parts.push(`state ${state.join(", ")}`);
  if (job.secrets.length > 0) parts.push(`secrets ${job.secrets.join(", ")} (none in run_jobs)`);
  // An unplaced job already says it has no place.
  if (!job.manual && job.placement === "") {
    parts.push(job.at.startsWith("machine:") ? "run_jobs cannot run it (Windows VM)" : "run_jobs cannot run it");
  }
  return parts.join("; ");
}

/** The runners and their state, then the machines (Windows VMs) when there are any. */
function runnersLine(view: RepoView): string {
  const named = (vm: boolean): string[] =>
    view.machines
      .filter((machine) => (machine.kind === "windows-vm") === vm)
      .map((machine) => `${machine.name} ${machine.status}`);
  const machines = named(true);
  const line = `runners: ${listed(named(false))}`;
  return machines.length > 0 ? `${line}; machines: ${machines.join(", ")}` : line;
}

/** A running job and how far it is into its expected time. */
export interface Lateness {
  job: string;
  place: string;
  elapsed: number;
  expected: number;
}

/** How far `job` of `view` is into its expected time at `now`; throws why there is no measure. */
export function lateJob(view: PipelineView, job: string, now: number): Lateness {
  const { repo, number } = view;
  const found = view.jobs.find((j) => j.name === job);
  if (!found) throw new Error(`no job ${job} in ${repo} #${number}`);
  if (found.status !== "running") throw new Error(`${job} of ${repo} #${number} is ${found.status}, not running`);
  const expected = found.expected_secs ?? 0;
  if (found.started == null || expected <= 0) {
    throw new Error(`${job} of ${repo} #${number} has no expected time: no succeeded run at its place yet`);
  }
  return { job, place: found.place, elapsed: Math.max(0, now - found.started), expected };
}

/**
 * The late report `zeughaus-runner ci overdue` prints: how long the job runs against its expected
 * time, what else its machine runs, its last lines, and what the agent can do about it.
 */
export function lateReport(
  view: PipelineView,
  late: Lateness,
  machines: MachineView[] | { error: string },
  log: string | { error: string },
): string {
  const { repo, number } = view;
  let text = `late: ${late.job} on ${late.place}: running ${late.elapsed} s, ${Math.floor(late.elapsed / late.expected)}x its expected ${late.expected} s (measured from its last successful runs)\n`;
  const isThis = (j: { repo: string; number: number; job: string }): boolean =>
    j.repo === repo && j.number === number && j.job === late.job;
  if ("error" in machines) {
    text += `  machine: not known (${machines.error})\n`;
  } else {
    const machine = machines.find((m) => m.jobs.some(isThis));
    if (!machine) {
      text += "  machine: not known (the CI lists no machine for it)\n";
    } else {
      const others = machine.jobs.filter((j) => !isThis(j)).map((j) => `${j.repo}#${j.number}/${j.job}`);
      text +=
        others.length === 0
          ? `  ${machine.name}: runs no other job\n`
          : `  ${machine.name}: also runs ${others.join(", ")}\n`;
    }
  }
  if (typeof log !== "string") {
    text += `  last lines: not available (${log.error})\n`;
  } else {
    text += "  last lines:\n";
    for (const line of lines(log)) text += `    ${line.trimEnd()}\n`;
  }
  text += `the CI stops it only at its timeout_minutes; to wait on call ci_wait repo=${repo} number=${number} (it answers again at the next multiple) or run \`zeughaus-runner ci wait ${repo} ${number}\` in the background; to stop it call ci_cancel repo=${repo} number=${number}\n`;
  return text;
}

/** The runners among the CI's machines, and the live sessions other than session `id`. */
export function sessionPeers(
  id: number,
  machines: MachineView[],
  sessions: SessionView[],
): [MachineView[], SessionView[]] {
  return [
    machines.filter((m) => m.kind === "runner"),
    sessions.filter((s) => s.id !== id && (s.state === "starting" || s.state === "running")),
  ];
}

/** What an agent session is told about itself and the runners around it. */
export function sessionText(session: SessionView, runners: MachineView[], others: SessionView[]): string {
  const doc = (s: SessionView): string => (s.doc != null ? ` #${s.doc}` : "");
  let text = `session ${session.id} ${session.kind} ${session.repo}${doc(session)} (${session.state})\n`;
  const host = runners.find((r) => r.name === session.runner);
  text += `runs on ${session.runner}: labels ${(host?.labels ?? []).join(",")}; addresses ${(host?.addresses ?? []).join(", ")}\n`;
  const machines = session.machines ?? [];
  if (machines.length > 0) {
    text += "machines:\n";
    for (const m of machines) text += `  ${m.name} ssh ${m.ssh ?? "-"} labels ${(m.labels ?? []).join(",")}\n`;
  }
  if (others.length > 0) {
    text += "other live sessions:\n";
    for (const o of others) text += `  #${o.id} ${o.kind} ${o.repo}${doc(o)} on ${o.runner}\n`;
  }
  text += "runners:\n";
  for (const r of runners) {
    text += `  ${r.name} ${r.status}${r.sessions ? " sessions" : ""} ${(r.labels ?? []).join(",")}\n`;
  }
  return text;
}

/** `ticket #3 of zeughaus`, `pipeline #12 of zeughaus`. */
export function subjectText(subject: ClaimSubject): string {
  return `${subject.on === "pipeline" ? "pipeline" : "ticket"} #${subject.number} of ${subject.repo}`;
}

/** `master zeughaus #38 on atik, terminal 7`, or `a session on atik, terminal 7` without a label. */
export function holderText(claim: ClaimView): string {
  return `${claim.label || "a session"} on ${claim.holder.runner}, terminal ${claim.holder.terminal}`;
}

/** What `ci claim` says: the subject is ours now, or who holds it. */
export function claimText(subject: ClaimSubject, claim: ClaimView, ours: boolean): string {
  return ours ? `claimed ${subjectText(subject)}` : `${subjectText(subject)} is held by ${holderText(claim)}`;
}

/** Who is responsible for pipeline `number` of `repo`, as `ci responsible` says it. */
export function responsibleText(repo: string, number: number, claim: ClaimView | null): string {
  if (!claim) return `nobody holds ${repo} #${number} or an earlier failure of its jobs`;
  if (claim.subject.number === number) return `responsible for ${repo} #${number}: ${holderText(claim)}`;
  return `responsible for ${repo} #${number}: ${holderText(claim)} (holds #${claim.subject.number})`;
}

/** The longest title of one ticket, in UTF-8 bytes. */
const MAX_DOC_TITLE_BYTES = 200;
/** How many lines under its title a plan may carry its header lines in. */
const HEADER_LINES = 20;

/** The title of a plan: its first `# ` heading, cut to 200 bytes on a character boundary. */
export function planTitle(body: string): string {
  const heading = lines(body).find((line) => line.startsWith("# "));
  const title = heading?.slice(2).trim() ?? "";
  if (title === "") return "Untitled plan";
  const encoder = new TextEncoder();
  let bytes = 0;
  let cut = "";
  for (const char of title) {
    bytes += encoder.encode(char).length;
    if (bytes > MAX_DOC_TITLE_BYTES) break;
    cut += char;
  }
  return cut.trimEnd();
}

/** The tickets an `Includes: #3, #5` line among the plan's first lines names; throws a malformed one. */
export function includesNamed(body: string): number[] {
  const line = lines(body)
    .slice(0, HEADER_LINES)
    .find((l) => l.startsWith("Includes:"));
  if (line === undefined) return [];
  const malformed = new Error(`malformed Includes line: ${line}`);
  const list = line.slice("Includes:".length).trim();
  if (list === "") throw malformed;
  return list.split(",").map((item) => {
    const digits = item.trim();
    if (!/^#[0-9]+$/.test(digits) || BigInt(digits.slice(1)) > 0xffff_ffff_ffff_ffffn) throw malformed;
    return Number(digits.slice(1));
  });
}

/** The label selectors a `Runs on: os=linux; os=windows` line names, one per machine; throws a malformed one. */
export function runsOnNamed(body: string): string[] {
  const line = lines(body)
    .slice(0, HEADER_LINES)
    .find((l) => l.startsWith("Runs on:"));
  if (line === undefined) return [];
  const malformed = new Error(`malformed Runs on line: ${line}`);
  const list = line.slice("Runs on:".length).trim();
  if (list === "") throw malformed;
  const selectors = list.split(";").map((part) => part.trim());
  for (const selector of selectors) {
    if (selector === "" || !selector.split(",").every((term) => validLabel(term.trim()))) throw malformed;
  }
  return selectors;
}

/** A runner label: `key` or `key=value`, the key `[a-z0-9][a-z0-9-]*`, the value `[A-Za-z0-9._-]+`. */
export function validLabel(label: string): boolean {
  const at = label.indexOf("=");
  const key = at < 0 ? label : label.slice(0, at);
  return validName(key) && (at < 0 || /^[A-Za-z0-9._-]+$/.test(label.slice(at + 1)));
}

/** The shape of a runner, repository or job name: `[a-z0-9][a-z0-9-]*`. */
export function validName(name: string): boolean {
  return /^[a-z0-9][a-z0-9-]*$/.test(name);
}

/** Whether `value` is an omp session id rather than a session file. */
export function isSessionId(value: string): boolean {
  return /^[A-Za-z0-9_-]+$/.test(value);
}
