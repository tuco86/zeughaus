// Reports what the main agent does to the zeughaus terminal it runs in. The runner reads the
// terminal user variable `zeughaus_agent` (iTerm2's `OSC 1337;SetUserVar`, base64 of a JSON
// object) and shows the state on the tab, as notices and on the CI page; zeughaus-link's
// `AgentStatus` is the reading side. Outside a zeughaus terminal (`ZEUGHAUS_TERMINAL` unset),
// without a TTY and outside the TUI nothing is written.

import { hostname } from "node:os";
import { masterPlan, planSession } from "./session-env.ts";

/** What a report needs of omp's handler context. */
interface ReportCtx {
  mode?: string;
  agent?: { kind: string };
}

/** The agent's state, as zeughaus-link's `AgentState` names it. */
export type AgentState = "idle" | "working" | "waiting" | "asking" | "review" | "failed" | "ended";

const VAR = "zeughaus_agent";
const MAX_DETAIL = 200;

export interface AgentReporter {
  report(ctx: ReportCtx, state: AgentState, detail?: string): void;
  setRepo(repo: string, doc: number | null): void;
}

export function agentReporter(): AgentReporter {
  let repo = "";
  let doc: number | null = null;
  // The last payload written without its time: an unchanged state is not written again.
  let previous = "";

  return {
    setRepo(name, number) {
      repo = name;
      doc = number;
    },
    report(ctx, state, detail = "") {
      try {
        if (!/^[0-9]+$/.test(process.env.ZEUGHAUS_TERMINAL ?? "")) return;
        if (!process.stdout.isTTY) return;
        if (ctx.agent && ctx.agent.kind !== "main") return;
        if (ctx.mode !== undefined && ctx.mode !== "tui") return;
        const status = {
          v: 1,
          state,
          role: masterPlan() ? "master" : planSession() ? "plan" : "work",
          repo,
          doc,
          detail: Array.from(detail).slice(0, MAX_DETAIL).join(""),
          pid: process.pid,
          host: hostname(),
        };
        const key = JSON.stringify(status);
        if (key === previous) return;
        previous = key;
        const json = JSON.stringify({ ...status, at: Date.now() });
        process.stdout.write(`\x1b]1337;SetUserVar=${VAR}=${Buffer.from(json).toString("base64")}\x07`);
      } catch {
        // A status nobody sees is no reason to disturb the session.
      }
    },
  };
}
