// What the environment says about the kind of session this omp runs: a master's, a plan
// session's, a session the CI opened.

/** `ZEUGHAUS_PLAN=<repo>#<n>`: the plan a master started this session on. */
export function masterPlan(): { repo: string; number: number } | undefined {
  const m = /^([a-z0-9][a-z0-9-]*)#([1-9][0-9]*)$/.exec(process.env.ZEUGHAUS_PLAN ?? "");
  return m ? { repo: m[1] as string, number: Number(m[2]) } : undefined;
}

/** `ZEUGHAUS_PLAN_SESSION=<repo>`, `ZEUGHAUS_PROPOSAL=<n>`: a planning session `zeughaus-runner plan` started. */
export function planSession(): { repo: string; proposal?: number } | undefined {
  const repo = process.env.ZEUGHAUS_PLAN_SESSION ?? "";
  if (!/^[a-z0-9][a-z0-9-]*$/.test(repo)) return undefined;
  const proposal = process.env.ZEUGHAUS_PROPOSAL ?? "";
  return /^[1-9][0-9]*$/.test(proposal) ? { repo, proposal: Number(proposal) } : { repo };
}

/** `ZEUGHAUS_SESSION=<id>`: a session the CI opened on a runner. */
export function sessionId(): number | undefined {
  const id = process.env.ZEUGHAUS_SESSION ?? "";
  return /^[1-9][0-9]*$/.test(id) ? Number(id) : undefined;
}
