// The CI's `/ci` API as this plugin speaks it (zeughaus-link/src/ci.rs): the requests and replies
// it uses, their limits, which requests may be asked twice, and where the CI and this machine's
// client key are named (zeughaus-link/src/credentials.rs, zeughaus-runner/src/mcp/client.rs).
// Pure: no fs, no process, no network.

import { posix, win32 } from "node:path";
import type { AskQuestion, AskReply } from "./ask.ts";

export const CI_PATH = "/ci";
/** The largest body of one ticket's proposal or plan, in bytes. */
export const MAX_DOC_BYTES = 256 << 10;
/** The largest request the CI reads. */
export const MAX_CI_REQUEST_BYTES = MAX_DOC_BYTES * 2 + (16 << 10);
/** The largest reply read from the CI. */
export const MAX_CI_REPLY_BYTES = 4 << 20;
/** Lines of a late job's output its report shows. */
export const LATE_TAIL = 10;

// The views, with the fields this plugin reads; snake_case as on the wire.
export interface JobView {
  name: string;
  needs: string[];
  place: string;
  status: string;
  note: string;
  started: number | null;
  finished: number | null;
  code: number | null;
  cause?: string | null;
  excerpt?: string | null;
  expected_secs?: number | null;
}
export interface PipelineView {
  repo: string;
  number: number;
  channel: string;
  event: string;
  git_ref: string;
  sha: string | null;
  status: string;
  note: string;
  created: number;
  finished: number | null;
  jobs: JobView[];
}
export interface ClaimSubject {
  on: "ticket" | "pipeline";
  repo: string;
  number: number;
}
export interface ClaimHolder {
  runner: string;
  terminal: number;
  session?: number;
}
export interface ClaimView {
  subject: ClaimSubject;
  holder: ClaimHolder;
  label?: string;
  channel?: string;
  since: number;
  renewed: number;
}
export interface ChannelView {
  repo: string;
  channel: string;
  latest: number;
  status: string;
  responsible?: ClaimView | null;
}
export interface JobDefView {
  name: string;
  file: string;
  on: string[];
  needs: string[];
  channels: string[];
  place: string;
  runner: string;
  at: string;
  placement: string;
  secrets: string[];
  state?: string[];
  manual: boolean;
}
export interface MachineJob {
  repo: string;
  channel: string;
  number: number;
  job: string;
}
export interface MachineView {
  name: string;
  kind: string;
  status: string;
  jobs: MachineJob[];
  labels?: string[];
  addresses?: string[];
  sessions?: boolean;
}
export interface DocView {
  repo: string;
  number: number;
  kind: "ticket" | "onboarding";
  title: string;
  state: string;
  source: string;
  created: number;
  changed: number;
}
export interface SessionMachine {
  name: string;
  ssh?: string | null;
  labels?: string[];
  addresses?: string[];
}
export interface SessionView {
  id: number;
  repo: string;
  kind: "plan" | "master";
  doc?: number | null;
  runner: string;
  machines?: SessionMachine[];
  state: "starting" | "running" | "ended" | "failed";
  started: number;
}
export interface RepoView {
  name: string;
  forge: string;
  default_branch: string;
  branch: string;
  channels: ChannelView[];
  head: PipelineView | null;
  jobs_sha: string | null;
  jobs: JobDefView[];
  jobs_note: string;
  machines: MachineView[];
  platforms: [string, string][];
  docs?: DocView[];
  setup?: string[];
}
export interface Planner {
  runner: string;
  omp_session: string;
}
export interface AskView {
  id: number;
  answer?: AskReply[] | null;
}

export type CiRequest =
  | { kind: "overview" }
  | { kind: "pipeline"; repo: string; number: number }
  | { kind: "find"; repo: string; sha: string }
  | {
      kind: "log";
      repo: string;
      number: number;
      job: string;
      tail: number;
      timestamps: boolean;
      lines: unknown[];
      pattern: string | null;
      context: number;
    }
  | { kind: "repo"; origin: string; sha: string | null; jobs: boolean; setup: boolean }
  | { kind: "doc"; repo: string; number: number }
  | {
      kind: "plan_write";
      repo: string;
      number: number | null;
      title: string;
      plan: string;
      includes: number[];
      runs_on: string[];
      source: string;
      planner: Planner | null;
    }
  | { kind: "session"; id: number }
  | { kind: "claim"; subject: ClaimSubject; holder: ClaimHolder }
  | { kind: "claim_renew"; holder: ClaimHolder }
  | { kind: "claim_release"; holder: ClaimHolder; subject: ClaimSubject | null }
  | { kind: "responsible"; repo: string; number: number }
  | { kind: "ask_post"; holder: ClaimHolder; repo: string; doc: number | null; questions: AskQuestion[] }
  | { kind: "ask_poll"; id: number; holder: ClaimHolder }
  | { kind: "ask_close"; holder: ClaimHolder };

export type CiReply =
  | { kind: "overview"; overview: { machines: MachineView[]; sessions?: SessionView[] } }
  | { kind: "pipeline"; pipeline: PipelineView | null }
  | { kind: "log"; text: string; finished: boolean }
  | { kind: "repo"; repo: RepoView | null; pending?: DocView | null }
  | { kind: "doc"; doc: DocView | null }
  | { kind: "doc_created"; number: number }
  | { kind: "session"; session: SessionView }
  | { kind: "claim"; claim: ClaimView; ours: boolean }
  | { kind: "responsible"; claim: ClaimView | null }
  | { kind: "ask"; ask: AskView }
  | { kind: "accepted" }
  | { kind: "error"; message: string };

/**
 * Whether `request` may be asked again after a broken exchange: only one that changes nothing.
 * No default branch, so a new request decides.
 */
export function retries(request: CiRequest): boolean {
  switch (request.kind) {
    case "overview":
    case "pipeline":
    case "find":
    case "log":
    case "repo":
    case "doc":
    case "session":
    case "responsible":
    // A renewal repeated is harmless.
    case "ask_poll":
      return true;
    case "plan_write":
    case "claim":
    case "claim_renew":
    case "claim_release":
    case "ask_post":
    case "ask_close":
      return false;
  }
}

export function encodeRequest(request: CiRequest): Uint8Array {
  const bytes = new TextEncoder().encode(JSON.stringify(request));
  if (bytes.length > MAX_CI_REQUEST_BYTES) {
    throw new Error(`the request is ${bytes.length} bytes, more than the CI reads (${MAX_CI_REQUEST_BYTES})`);
  }
  return bytes;
}

/** The CI's reply; its refusal (`error`) is thrown with the CI's message. */
export function decodeReply(bytes: Uint8Array): CiReply {
  let value: unknown;
  try {
    value = bytes.length > MAX_CI_REPLY_BYTES ? undefined : JSON.parse(new TextDecoder().decode(bytes));
  } catch {
    value = undefined;
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("ci: malformed reply");
  }
  const reply = value as { kind?: unknown; message?: unknown };
  if (typeof reply.kind !== "string") throw new Error("ci: malformed reply");
  if (reply.kind === "error") {
    throw new Error(typeof reply.message === "string" ? reply.message : "ci: malformed reply");
  }
  return value as CiReply;
}

/** `url` without the user and password of its authority: a remote may carry a token. */
export function withoutCredentials(url: string): string {
  const at = url.indexOf("://");
  if (at < 0) return url;
  const scheme = url.slice(0, at);
  const rest = url.slice(at + 3);
  const slash = rest.indexOf("/");
  const authority = slash < 0 ? rest : rest.slice(0, slash);
  const path = slash < 0 ? "" : rest.slice(slash);
  const userinfo = authority.lastIndexOf("@");
  const host = userinfo < 0 ? authority : authority.slice(userinfo + 1);
  return `${scheme}://${host}${path}`;
}

/**
 * The state directory: `ZEUGHAUS_STATE_DIR`, else `%LOCALAPPDATA%\zeughaus\state` on Windows,
 * else `$XDG_STATE_HOME/zeughaus` or `~/.local/state/zeughaus`, else `.zeughaus-state`. An empty
 * variable counts as unset.
 */
export function stateDir(env: Record<string, string | undefined>, platform: string): string {
  const set = (key: string): string | undefined => (env[key] ? env[key] : undefined);
  const explicit = set("ZEUGHAUS_STATE_DIR");
  if (explicit) return explicit;
  if (platform === "win32") {
    const local = set("LOCALAPPDATA");
    if (local) return win32.join(local, "zeughaus", "state");
  } else {
    const xdg = set("XDG_STATE_HOME");
    if (xdg) return posix.join(xdg, "zeughaus");
    const home = set("HOME");
    if (home) return posix.join(home, ".local", "state", "zeughaus");
  }
  return ".zeughaus-state";
}

interface ConfigFile {
  path: string;
  /** The file's text; `null` when there is no such file. */
  text: string | null;
}

function parseToml(file: ConfigFile & { text: string }): Record<string, unknown> {
  try {
    return Bun.TOML.parse(file.text) as Record<string, unknown>;
  } catch (e) {
    throw new Error(`${file.path}: ${e instanceof Error ? e.message : String(e)}`);
  }
}

/**
 * The CI to dial: `ZEUGHAUS_CI` when set, else the first of `ci = [..]` in the editor's
 * `zeughaus.toml`, else the `ci` of the runner's `enlist.toml`.
 */
export function pickCiUrl(envCi: string | undefined, zeughaus: ConfigFile, enlist: ConfigFile): string {
  if (envCi) return envCi;
  if (zeughaus.text !== null) {
    const ci = parseToml({ ...zeughaus, text: zeughaus.text }).ci ?? [];
    if (!Array.isArray(ci) || !ci.every((url) => typeof url === "string")) {
      throw new Error(`${zeughaus.path}: ci must be a list of URLs`);
    }
    if (ci.length > 0) return ci[0] as string;
  }
  if (enlist.text !== null) {
    const ci = parseToml({ ...enlist, text: enlist.text }).ci;
    if (typeof ci !== "string") throw new Error(`${enlist.path}: missing field ci`);
    return ci;
  }
  throw new Error(`no CI configured: add ci = [..] to ${zeughaus.path}`);
}

/** The `/ci` URL of the CI at `url`, which must pin the CI's fingerprint. */
export function ciUrl(url: string): string {
  const m = /^weida:\/\/(?:([^/@]*)@)?([^/]+)(?:\/.*)?$/.exec(url);
  if (!m || (m[1] !== undefined && !/^sha256:[0-9a-fA-F]{64}$/.test(m[1]))) {
    throw new Error(`the CI URL ${url} is not valid`);
  }
  if (m[1] === undefined) {
    throw new Error(
      `the CI URL ${url} names no fingerprint (weida://sha256:<fp>@host:port/): a CI that is not pinned could be anyone`,
    );
  }
  return `weida://${m[1]}@${m[2]}${CI_PATH}`;
}
