// What the plugin asks the CI, one function per question it used to put to a `zeughaus-runner
// ci` subcommand (zeughaus-runner/src/mcp/mod.rs, `ci_answer`): the requests, and the text the
// runner would have printed around the reply.

import { existsSync, readFileSync, realpathSync, statSync } from "node:fs";
import type { AskQuestion, AskReply } from "./ask.ts";
import {
  type CiReply,
  type CiRequest,
  type ClaimHolder,
  type ClaimSubject,
  type ClaimView,
  LATE_TAIL,
  type MachineView,
  type PipelineView,
  type RepoView,
} from "./protocol.ts";
import {
  claimText,
  describe,
  includesNamed,
  isSessionId,
  lateJob,
  lateReport,
  planTitle,
  responsibleText,
  runsOnNamed,
  sessionPeers,
  sessionText,
  validName,
} from "./text.ts";

/** Whatever answers a question on the CI's `/ci` path; `CiClient` in the plugin. */
export interface Ci {
  ask(request: CiRequest, signal?: AbortSignal): Promise<CiReply>;
}

/** The reply when it is of `kind`; else the CI answered something it was not asked. */
function expect<K extends CiReply["kind"]>(reply: CiReply, kind: K): Extract<CiReply, { kind: K }> {
  if (reply.kind !== kind) throw new Error(`the CI answered something else than asked: ${reply.kind}`);
  return reply as Extract<CiReply, { kind: K }>;
}

const message = (e: unknown): string => (e instanceof Error ? e.message : String(e));

/**
 * The CI's repository of the checkout whose origin is `origin` and whose HEAD is `head`, with
 * the text `ci repo` prints; `null` when no repository of the CI has that origin.
 */
export async function repoAnswer(
  ci: Ci,
  origin: string,
  head: string | null,
  signal?: AbortSignal,
): Promise<{ repo: RepoView; text: string } | null> {
  const request: CiRequest = { kind: "repo", origin, sha: head, jobs: true, setup: false };
  const { repo } = expect(await ci.ask(request, signal), "repo");
  return repo ? { repo, text: describe(repo, head) } : null;
}

/** The newest pipeline of a commit, `null` while there is none. */
export async function findPipeline(
  ci: Ci,
  repo: string,
  sha: string,
  signal?: AbortSignal,
): Promise<PipelineView | null> {
  return expect(await ci.ask({ kind: "find", repo, sha }, signal), "pipeline").pipeline;
}

/** The state of document `number` of `repo`. */
export async function docState(ci: Ci, repo: string, number: number, signal?: AbortSignal): Promise<string> {
  const { doc } = expect(await ci.ask({ kind: "doc", repo, number }, signal), "doc");
  if (!doc) throw new Error(`no document #${number} in ${repo}`);
  return doc.state;
}

/** The late report of a running job, as `ci overdue` prints it; throws when there is none. */
export async function overdueReport(
  ci: Ci,
  repo: string,
  number: number,
  job: string,
  signal?: AbortSignal,
): Promise<string> {
  const { pipeline } = expect(await ci.ask({ kind: "pipeline", repo, number }, signal), "pipeline");
  if (!pipeline) throw new Error(`no pipeline ${repo} #${number}`);
  const late = lateJob(pipeline, job, Math.floor(Date.now() / 1000));
  let machines: MachineView[] | { error: string };
  try {
    machines = expect(await ci.ask({ kind: "overview" }, signal), "overview").overview.machines;
  } catch (e) {
    machines = { error: message(e) };
  }
  let log: string | { error: string };
  try {
    const request: CiRequest = {
      kind: "log",
      repo,
      number,
      job,
      tail: LATE_TAIL,
      timestamps: false,
      lines: [],
      pattern: null,
      context: 0,
    };
    log = expect(await ci.ask(request, signal), "log").text;
  } catch (e) {
    log = { error: message(e) };
  }
  return lateReport(pipeline, late, machines, log);
}

/** What session `id` is told about itself and the runners around it, as `ci session` prints it. */
export async function describeSession(ci: Ci, id: number, signal?: AbortSignal): Promise<string> {
  const { session } = expect(await ci.ask({ kind: "session", id }, signal), "session");
  const { overview } = expect(await ci.ask({ kind: "overview" }, signal), "overview");
  return sessionText(session, ...sessionPeers(id, overview.machines, overview.sessions ?? []));
}

/** Takes `subject` for `holder`, or says who holds it. */
export async function takeClaim(
  ci: Ci,
  subject: ClaimSubject,
  holder: ClaimHolder,
  signal?: AbortSignal,
): Promise<{ ours: boolean; text: string }> {
  const { claim, ours } = expect(await ci.ask({ kind: "claim", subject, holder }, signal), "claim");
  return { ours, text: claimText(subject, claim, ours) };
}

/** Renews every claim of `holder`. */
export async function renewClaims(ci: Ci, holder: ClaimHolder, signal?: AbortSignal): Promise<void> {
  expect(await ci.ask({ kind: "claim_renew", holder }, signal), "accepted");
}

/** Releases `subject`, or every claim of `holder` with `null`. */
export async function releaseClaims(
  ci: Ci,
  holder: ClaimHolder,
  subject: ClaimSubject | null,
  signal?: AbortSignal,
): Promise<void> {
  expect(await ci.ask({ kind: "claim_release", holder, subject }, signal), "accepted");
}

/** Who is responsible for a pipeline's failure; `ours` when `holder` is that terminal. */
export async function whoIsResponsible(
  ci: Ci,
  repo: string,
  number: number,
  holder: ClaimHolder | undefined,
  signal?: AbortSignal,
): Promise<{ ours: boolean; claim: ClaimView | null; text: string }> {
  const { claim } = expect(await ci.ask({ kind: "responsible", repo, number }, signal), "responsible");
  const ours =
    holder !== undefined && claim !== null && claim.holder.runner === holder.runner && claim.holder.terminal === holder.terminal;
  return { ours, claim, text: responsibleText(repo, number, claim) };
}

/**
 * Stores the plan in the file at `path` on ticket `ticket` (which a plan session works on) or as
 * a new ticket; either is then in review. `ompSession` names the omp session a Refine resumes,
 * kept with this terminal's runner. Answers the ticket's number.
 */
export async function planWrite(
  ci: Ci,
  plan: { repo: string; ticket: number | null; path: string; ompSession: string | null },
  signal?: AbortSignal,
): Promise<number> {
  let body: string;
  try {
    body = readFileSync(plan.path, "utf8");
  } catch (e) {
    throw new Error(`cannot read ${plan.path}: ${message(e)}`);
  }
  if (body.trim() === "") throw new Error(`${plan.path} is empty`);
  const includes = includesNamed(body);
  const runsOn = runsOnNamed(body);
  // A Refine resumes the session on the runner that wrote the plan, so the session is kept only
  // together with that runner: its file by canonical path, or an omp session id as given.
  const runner = process.env.ZEUGHAUS_RUNNER ?? "";
  let ompSession: string | null = null;
  if (validName(runner) && plan.ompSession !== null) {
    if (existsSync(plan.ompSession) && statSync(plan.ompSession).isFile()) {
      ompSession = realpathSync(plan.ompSession);
    } else if (isSessionId(plan.ompSession)) {
      ompSession = plan.ompSession;
    }
  }
  const request: CiRequest = {
    kind: "plan_write",
    repo: plan.repo,
    number: plan.ticket,
    title: planTitle(body),
    plan: body,
    includes,
    runs_on: runsOn,
    source: plan.repo,
    planner: ompSession === null ? null : { runner, omp_session: ompSession },
  };
  return expect(await ci.ask(request, signal), "doc_created").number;
}

/** How often a posted ask is polled, which also renews its lease. */
const ASK_POLL_MS = 2_000;

/**
 * Posts `questions` for this terminal and waits, until `signal` ends it, for an editor's answer;
 * the CI drops an ask not polled for 30 s, so a CI that restarted or let it expire is asked again.
 */
export async function askInZeughaus(
  ci: Ci,
  holder: ClaimHolder,
  ask: { repo: string; doc: number | null; questions: AskQuestion[] },
  signal: AbortSignal,
): Promise<AskReply[]> {
  const post: CiRequest = { kind: "ask_post", holder, ...ask };
  let id = expect(await ci.ask(post, signal), "ask").ask.id;
  for (;;) {
    await Bun.sleep(ASK_POLL_MS);
    signal.throwIfAborted();
    try {
      const { ask: view } = expect(await ci.ask({ kind: "ask_poll", id, holder }, signal), "ask");
      if (view.answer) {
        await closeAsk(ci, holder).catch(() => undefined);
        return view.answer;
      }
    } catch (e) {
      signal.throwIfAborted();
      if (message(e).startsWith("no ask ")) {
        id = await ci
          .ask(post, signal)
          .then((reply) => expect(reply, "ask").ask.id)
          .catch(() => id);
      }
      // Anything else: the next poll tries a fresh connection.
    }
  }
}

/** Drops this terminal's ask: answered in the terminal, or no longer wanted. */
export async function closeAsk(ci: Ci, holder: ClaimHolder, signal?: AbortSignal): Promise<void> {
  expect(await ci.ask({ kind: "ask_close", holder }, signal), "accepted");
}
