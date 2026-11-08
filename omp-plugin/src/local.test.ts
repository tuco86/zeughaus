import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import { localFile, localRoots, safeSessionId } from "./local.ts";

const artifacts = join("home", "u", ".omp", "sessions", "s1");
const tmp = join("tmp");
const id = "01a1-b2";
const roots = localRoots(artifacts, id, tmp);
const inArtifacts = join(artifacts, "local", "x-plan.md");
const inTemp = join(tmp, "omp-local", id, "x-plan.md");

describe("localFile", () => {
  test("finds the file in omp's temp root when the artifacts root lacks it", () => {
    const files = new Set([inTemp]);
    expect(localFile("x-plan.md", roots, (p) => files.has(p))).toBe(inTemp);
  });

  test("names the artifacts path, which does not exist, when no root has the file", () => {
    const files = new Set<string>();
    const path = localFile("x-plan.md", roots, (p) => files.has(p));
    expect(path).toBe(inArtifacts);
    expect(files.has(path)).toBe(false);
  });

  test("prefers the artifacts root when both have the file", () => {
    const files = new Set([inArtifacts, inTemp]);
    expect(localFile("x-plan.md", roots, (p) => files.has(p))).toBe(inArtifacts);
  });
});

describe("localRoots", () => {
  test("without an artifacts directory only the temp root", () => {
    for (const none of [undefined, null, ""]) {
      expect(localRoots(none, id, tmp)).toEqual([join(tmp, "omp-local", id)]);
    }
  });
});

describe("safeSessionId", () => {
  test("keeps a safe id", () => {
    expect(safeSessionId("01a1-b2")).toBe("01a1-b2");
  });

  test("replaces unsafe characters", () => {
    expect(safeSessionId("a/b:c")).toBe("a_b_c");
  });

  test("falls back to session", () => {
    for (const none of [undefined, "", ".", ".."]) expect(safeSessionId(none)).toBe("session");
  });
});
