import { describe, expect, test } from "bun:test";
import { branchPushes, withTrailers } from "./shell.ts";

const T = ["Ticket: zeughaus#48"];

describe("withTrailers", () => {
  test("the trailer follows the commit word", () => {
    expect(withTrailers("git commit -m 'x' && git push", T)).toBe(
      "git commit --trailer 'Ticket: zeughaus#48' -m 'x' && git push",
    );
  });

  test("every git commit of the line gets it", () => {
    expect(withTrailers("git -C a commit -am x; git commit --amend --no-edit", T)).toBe(
      "git -C a commit --trailer 'Ticket: zeughaus#48' -am x; git commit --trailer 'Ticket: zeughaus#48' --amend --no-edit",
    );
  });

  test("a line that runs no git commit, and no trailers, change nothing", () => {
    expect(withTrailers("echo git commit", T)).toBe(undefined);
    expect(withTrailers("git log --grep commit", T)).toBe(undefined);
    expect(withTrailers("git commit -m x", [])).toBe(undefined);
  });

  test("a here-document's text is no command", () => {
    expect(withTrailers("git commit -F - <<'EOF'\ngit commit\nEOF", T)).toBe(
      "git commit --trailer 'Ticket: zeughaus#48' -F - <<'EOF'\ngit commit\nEOF",
    );
  });
});

test("branchPushes still finds a push", () => {
  expect(branchPushes("git push origin main")).toEqual([[]]);
});
