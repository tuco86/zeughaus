// The zeughaus-ci plugin for omp. In a checkout that zeughaus-ci builds it
//
// - asks the CI at session start (`zeughaus-runner ci repo --json`) and puts the answer into the
//   system prompt as a <zeughaus-ci> block and into the status line;
// - after a `git push` of this checkout polls `zeughaus-runner ci find --json` and reports the
//   first failed job of the pushed commit's pipeline into the session, or its success;
// - answers /zeughaus with the current state.
//
// The factory only registers: `omp plugin install` runs it against a throwaway API, so it must
// do no work at load. Every handler skips subagents, which have prompts of their own. Nothing
// here may reject out of a detached promise or a timer: omp ends the whole session on it.

import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";

// The parts of omp's extension API this plugin uses; omp ships no type package to import.
interface Exec {
  stdout: string;
  stderr: string;
  code: number;
  killed: boolean;
}
interface Ctx {
  cwd: string;
  hasUI: boolean;
  agent?: { kind: string };
  ui: {
    notify(message: string, level: "info" | "warning" | "error"): void;
    setStatus(key: string, text: string | undefined): void;
  };
  setInterval(callback: () => unknown, ms: number): unknown;
  clearTimer(timer: unknown): void;
}
interface ToolResultEvent {
  toolName: string;
  input: Record<string, unknown>;
  content: Array<{ type: string; text?: string }>;
  isError: boolean;
}
interface Events {
  session_start: (event: unknown, ctx: Ctx) => void;
  session_switch: (event: unknown, ctx: Ctx) => void;
  before_agent_start: (
    event: { systemPrompt: string[] },
    ctx: Ctx,
  ) => Promise<{ systemPrompt: string[] } | undefined>;
  tool_result: (
    event: ToolResultEvent,
    ctx: Ctx,
  ) => Promise<{ additionalContext: string } | undefined>;
}
interface Pi {
  on<E extends keyof Events>(event: E, handler: Events[E]): void;
  exec(
    command: string,
    args: string[],
    options: { cwd: string; signal: AbortSignal },
  ): Promise<Exec>;
  sendMessage(
    message: { customType: string; content: string; display: boolean },
    options: { deliverAs: "aside" | "nextTurn"; triggerTurn?: boolean },
  ): void;
  registerCommand(
    name: string,
    command: { description: string; handler: (args: string, ctx: Ctx) => Promise<void> },
  ): void;
}

// What `zeughaus-runner ci` prints (zeughaus-link/src/ci.rs); only the fields used here.
interface JobView {
  name: string;
  place: string;
  status: string;
  note?: string;
  code?: number | null;
  cause?: string | null;
  excerpt?: string | null;
}
interface PipelineView {
  number: number;
  channel: string;
  status: string;
  note?: string;
  jobs: JobView[];
}
interface RepoView {
  name: string;
  branch: string;
  head?: { status: string } | null;
}

type CiContext =
  | { kind: "none" }
  | { kind: "repo"; root: string; view: RepoView; text: string; head: string }
  | { kind: "unknown"; root: string }
  | { kind: "unreachable"; root: string; reason: string };

/** The pipeline of a pushed commit as far as the poll has seen it. */
interface Progress {
  sha7: string;
  pipeline?: PipelineView;
}
interface Watch {
  ctx: Ctx;
  name: string;
  root: string;
  sha: string;
  started: number;
  timer: unknown;
  busy: boolean;
  failures: number;
}

const RUNNER = "zeughaus-runner";
const CONTEXT_TIMEOUT_MS = 15_000;
const FIND_TIMEOUT_MS = 10_000;
const GIT_TIMEOUT_MS = 10_000;
const POLL_MS = 15_000;
const APPEAR_LIMIT_MS = 10 * 60_000;
const WATCH_LIMIT_MS = 5 * 60 * 60_000;
const MAX_FIND_FAILURES = 3;
const EXCERPT_LINES = 30;
const KEY = "zeughaus-ci"; // status key and message customType
const OPEN = "<zeughaus-ci>";
const CLOSE = "</zeughaus-ci>";

/** Runs a program; one that cannot start (not installed) answers like a shell, with 127. */
async function exec(
  pi: Pi,
  command: string,
  args: string[],
  cwd: string,
  ms: number,
): Promise<Exec> {
  try {
    const r = await pi.exec(command, args, { cwd, signal: AbortSignal.timeout(ms) });
    return { stdout: r.stdout, stderr: r.stderr, code: r.code, killed: r.killed };
  } catch (e) {
    return { stdout: "", stderr: String(e), code: 127, killed: false };
  }
}

/** Why `zeughaus-runner` gave no answer. */
function failure(r: Exec, what: string): string {
  if (r.code === 127) return `${RUNNER} not found`;
  const line = r.stderr.split("\n").find((l) => l.trim() !== "");
  return line?.trim() || (r.killed ? `${what} timed out` : `${RUNNER} exited with ${r.code}`);
}

function parseRepo(stdout: string): { repo: RepoView; text: string } | undefined {
  try {
    const v = JSON.parse(stdout) as { repo?: RepoView; text?: unknown };
    const { repo, text } = v;
    if (repo && typeof repo.name === "string" && typeof repo.branch === "string") {
      if (typeof text === "string") return { repo, text };
    }
  } catch {
    // Not JSON: the caller reports an unreadable answer.
  }
  return undefined;
}

/** The pipeline, `null` when the CI has none for the commit, `undefined` for an unreadable answer. */
function parsePipeline(stdout: string): PipelineView | null | undefined {
  try {
    const v = JSON.parse(stdout) as PipelineView | null;
    if (v === null) return null;
    if (typeof v.number === "number" && typeof v.status === "string" && Array.isArray(v.jobs)) {
      return v;
    }
  } catch {
    // Not JSON: the caller counts a failed poll.
  }
  return undefined;
}

/** What the CI says about the checkout of `cwd`; never rejects. */
async function loadContext(pi: Pi, cwd: string): Promise<CiContext> {
  const top = await exec(pi, "git", ["rev-parse", "--show-toplevel"], cwd, GIT_TIMEOUT_MS);
  const root = top.stdout.trim();
  if (top.code !== 0 || root === "" || !existsSync(join(root, ".zeughaus-ci"))) {
    return { kind: "none" };
  }
  const r = await exec(pi, RUNNER, ["ci", "repo", "--json", root], root, CONTEXT_TIMEOUT_MS);
  if (r.code === 3) return { kind: "unknown", root };
  if (r.code !== 0) return { kind: "unreachable", root, reason: failure(r, "ci repo") };
  const answer = parseRepo(r.stdout);
  if (!answer) {
    return { kind: "unreachable", root, reason: `${RUNNER} gave an unreadable answer` };
  }
  const head = answer.repo.head?.status ?? "unbuilt";
  return { kind: "repo", root, view: answer.repo, text: answer.text.trim(), head };
}

const wrap = (...lines: string[]): string => [OPEN, ...lines, CLOSE].join("\n");

/** The system prompt block; the same text for the whole session, so the prompt cache holds. */
function block(c: CiContext): string | undefined {
  switch (c.kind) {
    case "none":
      return undefined;
    case "repo":
      return wrap(
        "zeughaus-ci, the user's own CI, builds this checkout (state at session start; ci_repo for now):",
        c.text,
        "Tests before a push: only the unit tier of the crates or packages the change touches (Rust: cargo nextest run -p <crate>; Python: uv run pytest tests/unit). Never the whole suite, the integration or the end-to-end tier locally: the push pipeline runs what its jobs select right after the push, and integration and end-to-end tests belong to the nightly and release pipelines.",
        "Builds and tests on another platform go through run_jobs, never by hand over ssh; skill://zeughaus-ci says how to debug a job or platform with it and how jobs are written.",
      );
    case "unknown":
      return wrap(
        "This checkout has .zeughaus-ci/ but its origin is no repository of the CI's ci.toml: pushes start no pipeline and run_jobs is refused.",
      );
    case "unreachable":
      return wrap(
        `This checkout has .zeughaus-ci/; the CI could not be asked at session start: ${c.reason}`,
      );
  }
}

function statusText(c: CiContext, progress?: Progress): string | undefined {
  switch (c.kind) {
    case "none":
      return undefined;
    case "unknown":
      return "CI: unknown repo";
    case "unreachable":
      return "CI unreachable";
    case "repo": {
      const p = progress?.pipeline;
      if (progress && p) {
        const done = p.jobs.filter((job) =>
          ["succeeded", "failed", "skipped"].includes(job.status),
        );
        return `CI ${c.view.name}: #${p.number} ${p.status} ${done.length}/${p.jobs.length}`;
      }
      if (progress) return `CI ${c.view.name}: waiting for ${progress.sha7}`;
      return `CI ${c.view.name}: ${c.view.branch === "green" ? "green" : "red"} | HEAD ${c.head}`;
    }
  }
}

/** What /zeughaus tells for a context. */
function explain(c: CiContext): string {
  switch (c.kind) {
    case "none":
      return "This checkout has no .zeughaus-ci/.";
    case "repo":
      return c.text;
    case "unknown":
      return "This checkout's origin is no repository of the CI's ci.toml: pushes start no pipeline and run_jobs is refused.";
    case "unreachable":
      return `The CI could not be asked: ${c.reason}`;
  }
}

function failedMessage(name: string, sha7: string, p: PipelineView, job: JobView): string {
  const why =
    job.cause?.trim() ||
    job.note?.trim() ||
    (job.code == null ? "no cause recorded" : `exit code ${job.code}`);
  const lines = [
    `zeughaus-ci: ${name} #${p.number} (${sha7}, ${p.channel}): ${job.name} failed on ${job.place}: ${why}`,
  ];
  const excerpt = (job.excerpt ?? "").trimEnd().split("\n").slice(-EXCERPT_LINES).join("\n");
  if (excerpt) lines.push(excerpt);
  lines.push(
    `Next: ci_log number=${p.number} job=${job.name} for its output; run_jobs jobs=["${job.name}"] reproduces it from the working tree.`,
  );
  return lines.join("\n");
}

/**
 * A pipeline that ended without a failed job: `error`, `cancelled`, `no-jobs`, and a `failed`
 * one whose jobs were all skipped (a runner that stayed offline), where the first skipped job
 * says why.
 */
function endedMessage(name: string, sha7: string, p: PipelineView): string {
  const skipped = p.jobs.find((job) => job.status === "skipped" && job.note?.trim());
  const note = p.note?.trim() || (skipped ? `${skipped.name}: ${skipped.note?.trim()}` : "");
  return `zeughaus-ci: ${name} #${p.number} (${sha7}) ${p.status}${note ? `: ${note}` : ""}`;
}

/** A pipeline succeeds only when every job did: one skipped job makes it `failed`. */
function succeededMessage(name: string, sha7: string, p: PipelineView): string {
  const jobs = p.jobs.length;
  return `zeughaus-ci: ${name} #${p.number} (${sha7}, ${p.channel}) succeeded: ${jobs} job${jobs === 1 ? "" : "s"}.`;
}

// ---- Which bash commands push this checkout --------------------------------------------------

interface Heredoc {
  tag: string;
  indent: boolean;
}

/**
 * The simple commands of a shell line, as lists of words with quotes and escapes resolved. It is
 * no shell: it knows quoting, comments, here-documents and the separators `; & | ( )` and
 * newline, which is what it takes to tell what a line runs, as opposed to what it only mentions
 * in a commit message or an echo.
 */
function simpleCommands(line: string): string[][] {
  const commands: string[][] = [];
  const heredocs: Heredoc[] = [];
  let words: string[] = [];
  let word: string | undefined;
  let heredoc: Heredoc | undefined; // `<<` seen: the next word is its delimiter
  let i = 0;

  const endWord = (): void => {
    if (word === undefined) return;
    if (heredoc) {
      heredocs.push({ tag: word, indent: heredoc.indent });
      heredoc = undefined;
    } else {
      words.push(word);
    }
    word = undefined;
  };
  const endCommand = (): void => {
    endWord();
    if (words.length > 0) commands.push(words);
    words = [];
  };

  while (i < line.length) {
    const ch = line.charAt(i);
    if (ch === "'") {
      const close = line.indexOf("'", i + 1);
      const end = close === -1 ? line.length : close;
      word = (word ?? "") + line.slice(i + 1, end);
      i = end + 1;
    } else if (ch === '"') {
      let text = "";
      i += 1;
      while (i < line.length && line.charAt(i) !== '"') {
        if (
          line.charAt(i) === "\\" &&
          i + 1 < line.length &&
          '"\\$`\n'.includes(line.charAt(i + 1))
        ) {
          i += 1;
        }
        text += line.charAt(i);
        i += 1;
      }
      word = (word ?? "") + text;
      i += 1;
    } else if (ch === "\\") {
      if (line.charAt(i + 1) !== "\n") word = (word ?? "") + line.charAt(i + 1);
      i += 2;
    } else if (ch === "\n") {
      endCommand();
      i += 1;
      for (const h of heredocs) {
        while (i < line.length) {
          const end = line.indexOf("\n", i);
          const text = line.slice(i, end === -1 ? line.length : end);
          i = end === -1 ? line.length : end + 1;
          if ((h.indent ? text.replace(/^\t+/, "") : text) === h.tag) break;
        }
      }
      heredocs.length = 0;
    } else if (ch === " " || ch === "\t" || ch === "\r") {
      endWord();
      i += 1;
    } else if (ch === "#" && word === undefined) {
      const end = line.indexOf("\n", i);
      i = end === -1 ? line.length : end;
    } else if (ch === "<" && line.startsWith("<<", i) && line.charAt(i + 2) !== "<") {
      endWord();
      heredoc = { tag: "", indent: line.charAt(i + 2) === "-" };
      i += heredoc.indent ? 3 : 2;
    } else if (
      ch === "&" &&
      (word?.endsWith(">") || word?.endsWith("<") || line.charAt(i + 1) === ">")
    ) {
      word = (word ?? "") + ch; // `2>&1`, `&>file`: a redirection, not a separator
      i += 1;
    } else if (";|&()`".includes(ch)) {
      endCommand();
      i += 1;
    } else {
      word = (word ?? "") + ch;
      i += 1;
    }
  }
  endCommand();
  return commands;
}

// Words that may stand before a command without being it.
const PREFIX =
  /^(?:if|then|else|elif|do|while|until|!|\{|\}|time|command|exec|nohup|sudo|env|builtin)$/;
const ASSIGNMENT = /^[A-Za-z_][A-Za-z0-9_]*=/;
const REDIRECT = /^(?:\d*|&)(?:>>?|<)/;
const REDIRECT_ONLY = /^(?:\d*|&)(?:>>?|<)$/;
const TAKES_VALUE = /^(?:-o|--push-option|--repo|--receive-pack|--exec)$/;

/** The `-C` directories and the arguments of a command that is `git push`. */
function gitPush(words: string[]): { dirs: string[]; args: string[] } | undefined {
  let k = 0;
  while (k < words.length) {
    const w = words[k] ?? "";
    if (ASSIGNMENT.test(w) || PREFIX.test(w)) k += 1;
    else if (w === "timeout" && k + 1 < words.length) k += 2;
    else break;
  }
  const program = words[k];
  if (program !== "git" && !program?.endsWith("/git")) return undefined;
  k += 1;
  const dirs: string[] = [];
  while (k < words.length && words[k] !== "push") {
    const w = words[k] ?? "";
    if (w === "-C" && k + 1 < words.length) {
      dirs.push(words[k + 1] ?? "");
      k += 2;
    } else if (w === "-c" && k + 1 < words.length) {
      k += 2;
    } else if (w.startsWith("-")) {
      k += 1;
    } else {
      return undefined; // another subcommand
    }
  }
  if (words[k] !== "push") return undefined;
  return { dirs, args: words.slice(k + 1) };
}

/**
 * Whether the arguments of a `git push` can start a pipeline of the checked-out commit. Not so
 * for a dry run, a deletion, and a push of tags only (`--tags` without a refspec).
 */
function pushesBranch(args: string[]): boolean {
  const flags: string[] = [];
  const positional: string[] = [];
  let rest = false; // after `--`
  for (let i = 0; i < args.length; i++) {
    const a = args[i] ?? "";
    if (!rest && REDIRECT.test(a)) {
      if (REDIRECT_ONLY.test(a)) i += 1; // its target is the next word
    } else if (!rest && a === "--") {
      rest = true;
    } else if (!rest && a.startsWith("-")) {
      flags.push(a);
      if (TAKES_VALUE.test(a)) i += 1;
    } else {
      positional.push(a);
    }
  }
  const has = (pattern: RegExp): boolean => flags.some((f) => pattern.test(f));
  if (has(/^--dry-run$/) || has(/^-[A-Za-z]*n[A-Za-z]*$/)) return false;
  if (has(/^--delete$/) || has(/^-[A-Za-z]*d[A-Za-z]*$/)) return false;
  if (has(/^--tags$/) && positional.length <= 1) return false;
  return true;
}

/**
 * For every `git push` of a bash command that can start a pipeline, the directories it runs in
 * relative to the command's own: the `cd` targets before it, then its `-C` options.
 */
function branchPushes(command: string): string[][] {
  const found: string[][] = [];
  const cds: string[] = [];
  for (const words of simpleCommands(command)) {
    if (words[0] === "cd" && words[1] !== undefined) {
      cds.push(words[1]);
      continue;
    }
    const push = gitPush(words);
    if (push && pushesBranch(push.args)) found.push([...cds, ...push.dirs]);
  }
  return found;
}

/** HEAD of the checkout at `root` when one of the pushes ran in it, else `undefined`. */
async function pushedHead(
  pi: Pi,
  root: string,
  base: string,
  pushes: string[][],
): Promise<string | undefined> {
  for (const dirs of pushes) {
    const dir = dirs.reduce((from, d) => {
      // The shell expands a leading `~` before git sees the word.
      const target = d === "~" || d.startsWith("~/") ? join(homedir(), d.slice(1)) : d;
      return resolve(from, target);
    }, base);
    const r = await exec(pi, "git", ["rev-parse", "--show-toplevel", "HEAD"], dir, GIT_TIMEOUT_MS);
    const [top, sha] = r.stdout.split("\n");
    if (r.code === 0 && top === root && sha) return sha.trim();
  }
  return undefined;
}

// ---- The extension --------------------------------------------------------------------------

export default function zeughausCi(pi: Pi): void {
  // What the system prompt block is built from, per working directory.
  let loaded: { cwd: string; context: Promise<CiContext> } | undefined;
  // The newest answer of the CI, for the status line and /zeughaus.
  let latest: CiContext = { kind: "none" };
  let progress: Progress | undefined;
  let watch: Watch | undefined;

  const isMain = (ctx: Ctx): boolean => !ctx.agent || ctx.agent.kind === "main";

  const contextFor = (cwd: string): Promise<CiContext> => {
    if (!loaded || loaded.cwd !== cwd) loaded = { cwd, context: loadContext(pi, cwd) };
    return loaded.context;
  };

  const showStatus = (ctx: Ctx): void => {
    if (ctx.hasUI) ctx.ui.setStatus(KEY, statusText(latest, progress));
  };

  const prime = async (ctx: Ctx): Promise<void> => {
    latest = await contextFor(ctx.cwd);
    showStatus(ctx);
  };

  /** Ends the watch. A result that is news keeps its status in the line until the next push. */
  const endWatch = (w: Watch, keepStatus: boolean): void => {
    if (watch !== w) return;
    w.ctx.clearTimer(w.timer);
    watch = undefined;
    if (!keepStatus) {
      progress = undefined;
      showStatus(w.ctx);
    }
  };

  const send = (content: string, deliverAs: "aside" | "nextTurn", triggerTurn: boolean): void => {
    pi.sendMessage({ customType: KEY, content, display: true }, { deliverAs, triggerTurn });
  };

  /** A pipeline of the pushed commit: reports a failure or the end, else keeps watching. */
  const report = (w: Watch, p: PipelineView): void => {
    const sha7 = w.sha.slice(0, 7);
    const failed = p.jobs.find((job) => job.status === "failed");
    // With a failed job the pipeline cannot succeed any more; the line keeps what the watch ends on.
    progress = { sha7, pipeline: failed ? { ...p, status: "failed" } : p };
    showStatus(w.ctx);
    if (failed) {
      const message = failedMessage(w.name, sha7, p, failed);
      send(message, "aside", true);
      w.ctx.ui.notify(message.split("\n")[0] ?? message, "error");
      endWatch(w, true);
    } else if (p.status === "error" || p.status === "cancelled" || p.status === "failed") {
      send(endedMessage(w.name, sha7, p), "aside", true);
      endWatch(w, true);
    } else if (p.status === "no-jobs") {
      const message = endedMessage(w.name, sha7, p);
      send(message, "nextTurn", false);
      w.ctx.ui.notify(message, "warning");
      endWatch(w, true);
    } else if (p.status === "succeeded") {
      const message = succeededMessage(w.name, sha7, p);
      send(message, "nextTurn", false);
      w.ctx.ui.notify(message, "info");
      endWatch(w, true);
    }
  };

  const poll = async (w: Watch): Promise<void> => {
    if (w.busy || watch !== w) return;
    w.busy = true;
    try {
      if (Date.now() - w.started >= WATCH_LIMIT_MS) {
        endWatch(w, false);
        return;
      }
      const args = ["ci", "find", "--json", w.name, w.sha];
      const r = await exec(pi, RUNNER, args, w.root, FIND_TIMEOUT_MS);
      if (watch !== w) return;
      const sha7 = w.sha.slice(0, 7);
      const p = r.code === 0 ? parsePipeline(r.stdout) : undefined;
      if (p === undefined) {
        w.failures += 1;
        if (w.failures >= MAX_FIND_FAILURES) {
          const why = r.code === 0 ? "unreadable answer" : failure(r, "ci find");
          w.ctx.ui.notify(`zeughaus-ci: cannot watch ${sha7}: ${why}`, "warning");
          endWatch(w, false);
        }
        return;
      }
      w.failures = 0;
      if (p !== null) {
        report(w, p);
      } else if (Date.now() - w.started >= APPEAR_LIMIT_MS) {
        send(
          `zeughaus-ci: no pipeline started for ${sha7} in ${w.name} within 10 min: no job's \`on\` selects this ref, or the push did not reach the CI.`,
          "aside",
          true,
        );
        endWatch(w, false);
      }
    } finally {
      w.busy = false;
    }
  };

  /** Starts watching the pipeline of `sha`, replacing a running watch. */
  const startWatch = (ctx: Ctx, name: string, root: string, sha: string): void => {
    if (watch) endWatch(watch, false);
    const w: Watch = {
      ctx,
      name,
      root,
      sha,
      started: Date.now(),
      timer: undefined,
      busy: false,
      failures: 0,
    };
    watch = w;
    progress = { sha7: sha.slice(0, 7) };
    showStatus(ctx);
    w.timer = ctx.setInterval(() => poll(w), POLL_MS);
  };

  pi.on("session_start", (_event, ctx) => {
    if (!isMain(ctx)) return;
    // Not awaited: a slow CI must not hold up the session; before_agent_start waits for it.
    void prime(ctx).catch(() => undefined);
  });

  // A new or resumed session in this process: its prompt block and status are its own, and the
  // watch of the previous one must not report into it.
  pi.on("session_switch", (_event, ctx) => {
    if (!isMain(ctx)) return;
    if (watch) endWatch(watch, false);
    loaded = undefined;
    latest = { kind: "none" };
    progress = undefined;
    showStatus(ctx);
    void prime(ctx).catch(() => undefined);
  });

  pi.on("before_agent_start", async (event, ctx) => {
    if (!isMain(ctx) || !Array.isArray(event.systemPrompt)) return undefined;
    const text = block(await contextFor(ctx.cwd));
    // Exact match: AGENTS.md and the skill mention the tag, so a substring test would skip the block.
    if (text === undefined || event.systemPrompt.includes(text)) return undefined;
    return { systemPrompt: [...event.systemPrompt, text] };
  });

  pi.on("tool_result", async (event, ctx) => {
    if (!isMain(ctx) || event.toolName !== "bash" || event.isError) return undefined;
    const command = event.input.command;
    if (typeof command !== "string") return undefined;
    const output = Array.isArray(event.content)
      ? event.content.map((p) => p.text ?? "").join("\n")
      : "";
    const pushes = branchPushes(command);
    if (pushes.length === 0 || output.includes("Everything up-to-date")) return undefined;
    const c = await contextFor(ctx.cwd);
    if (c.kind !== "repo") return undefined;
    const cwd = event.input.cwd;
    const base = typeof cwd === "string" ? resolve(ctx.cwd, cwd) : ctx.cwd;
    const sha = await pushedHead(pi, c.root, base, pushes);
    if (sha === undefined) return undefined;
    startWatch(ctx, c.view.name, c.root, sha);
    return {
      additionalContext: `zeughaus-ci watches the pipeline of ${sha.slice(0, 7)} in ${c.view.name} and reports its first failure, or its success, here: no need to poll.`,
    };
  });

  pi.registerCommand("zeughaus", {
    description: "zeughaus-ci state of this checkout",
    handler: async (_args, ctx) => {
      // Only the status line and this answer are fresh; the prompt block stays as it was.
      latest = await loadContext(pi, ctx.cwd);
      if (!watch) progress = undefined;
      showStatus(ctx);
      ctx.ui.notify(explain(latest), "info");
    },
  });
}
