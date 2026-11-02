// What the environment says about the kind of session this omp runs: a master's, a plan
// session's, a session the CI opened.

/** `ZEUGHAUS_PLAN=<repo>#<n>`: the plan a master started this session on. */
export function masterPlan(): { repo: string; number: number } | undefined {
  const m = /^([a-z0-9][a-z0-9-]*)#([1-9][0-9]*)$/.exec(process.env.ZEUGHAUS_PLAN ?? "");
  return m ? { repo: m[1] as string, number: Number(m[2]) } : undefined;
}

/** `ZEUGHAUS_PLAN_SESSION=<repo>`, `ZEUGHAUS_TICKET=<n>`: a planning session `zeughaus-runner plan` started. */
export function planSession(): { repo: string; ticket?: number } | undefined {
  const repo = process.env.ZEUGHAUS_PLAN_SESSION ?? "";
  if (!/^[a-z0-9][a-z0-9-]*$/.test(repo)) return undefined;
  const ticket = process.env.ZEUGHAUS_TICKET ?? "";
  return /^[1-9][0-9]*$/.test(ticket) ? { repo, ticket: Number(ticket) } : { repo };
}

/** `ZEUGHAUS_SESSION=<id>`: a session the CI opened on a runner. */
export function sessionId(): number | undefined {
  const id = process.env.ZEUGHAUS_SESSION ?? "";
  return /^[1-9][0-9]*$/.test(id) ? Number(id) : undefined;
}

/** `ZEUGHAUS_RUNNER`, `ZEUGHAUS_TERMINAL`: the zeughaus terminal this omp runs in, which holds its claims. */
export function claimHolder(): { runner: string; terminal: number } | undefined {
  const runner = process.env.ZEUGHAUS_RUNNER ?? "";
  const terminal = process.env.ZEUGHAUS_TERMINAL ?? "";
  if (!/^[a-z0-9][a-z0-9-]*$/.test(runner) || !/^[0-9]+$/.test(terminal)) return undefined;
  return { runner, terminal: Number(terminal) };
}
