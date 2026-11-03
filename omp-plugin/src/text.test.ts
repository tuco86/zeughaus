import { describe, expect, test } from "bun:test";
import claims from "../fixtures/claims.json";
import describeCases from "../fixtures/describe.json";
import late from "../fixtures/late.json";
import planHeader from "../fixtures/plan-header.json";
import session from "../fixtures/session.json";
import type { ClaimSubject, ClaimView, MachineView, PipelineView, RepoView, SessionView } from "./protocol.ts";
import {
  claimText,
  describe as describeRepo,
  includesNamed,
  lateJob,
  lateReport,
  planTitle,
  responsibleText,
  runsOnNamed,
  sessionPeers,
  sessionText,
} from "./text.ts";

// The cases zeughaus-runner's and zeughaus-link's tests run against the Rust originals.

describe("describe", () => {
  for (const c of describeCases) {
    test(c.name, () => {
      expect(describeRepo(c.view as unknown as RepoView, c.head)).toBe(c.text);
    });
  }
});

describe("late report", () => {
  for (const c of late as Array<Record<string, unknown> & { name: string; job: string; now: number }>) {
    test(c.name, () => {
      const view = c.view as PipelineView;
      if (typeof c.error === "string") {
        expect(() => lateJob(view, c.job, c.now)).toThrow(c.error);
        return;
      }
      const machines = (c.machines as MachineView[] | undefined) ?? { error: c.machines_error as string };
      const log = (c.log as string | undefined) ?? { error: c.log_error as string };
      expect(lateReport(view, lateJob(view, c.job, c.now), machines, log)).toBe(c.text as string);
    });
  }
});

describe("session", () => {
  for (const c of session) {
    test(c.name, () => {
      const me = c.session as unknown as SessionView;
      const [runners, others] = sessionPeers(
        me.id,
        c.machines as unknown as MachineView[],
        c.sessions as unknown as SessionView[],
      );
      expect(sessionText(me, runners, others)).toBe(c.text);
    });
  }
});

describe("claims", () => {
  for (const c of claims.claim) {
    test(c.name, () => {
      expect(claimText(c.subject as ClaimSubject, c.claim as ClaimView, c.ours)).toBe(c.text);
    });
  }
  for (const c of claims.responsible) {
    test(`responsible: ${c.name}`, () => {
      expect(responsibleText(c.repo, c.number, c.claim as ClaimView | null)).toBe(c.text);
    });
  }
});

describe("plan header", () => {
  for (const c of planHeader as Array<Record<string, unknown> & { name: string; body: string; title: string }>) {
    test(c.name, () => {
      expect(planTitle(c.body)).toBe(c.title);
      if (typeof c.includes_error === "string") expect(() => includesNamed(c.body)).toThrow(c.includes_error);
      else expect(includesNamed(c.body)).toEqual(c.includes as number[]);
      if (typeof c.runs_on_error === "string") expect(() => runsOnNamed(c.body)).toThrow(c.runs_on_error);
      else expect(runsOnNamed(c.body)).toEqual(c.runs_on as string[]);
    });
  }
});
