import { describe, expect, test } from "bun:test";
import { rewrite } from "./secrets.ts";

describe("rewrite", () => {
  test("one variable per document and field, in one subshell with one read", () => {
    const out = rewrite(
      "curl -H \"A: ${secret:hannes/omp/F}\" -H \"B: ${secret:hannes/omp/x/F}\" -d ${secret:hannes/omp/F}",
    );
    expect(out).toEqual({
      command:
        "( ZHS__=$(get-secret --export 'ZHS_F=secret/hannes/omp/F' 'ZHS_F_2=secret/hannes/omp/x/F') || exit 1; eval \"$ZHS__\"; unset ZHS__\n" +
        'curl -H "A: ${ZHS_F}" -H "B: ${ZHS_F_2}" -d ${ZHS_F}\n)',
    });
  });

  test("a placeholder outside hannes/omp is refused", () => {
    expect(rewrite("echo ${secret:hannes/shell/X}")).toEqual({
      refused: "only secrets under hannes/omp resolve in omp sessions: hannes/shell/X",
    });
    expect(rewrite("echo ${secret:hannes/ompx/X}")).toEqual({
      refused: "only secrets under hannes/omp resolve in omp sessions: hannes/ompx/X",
    });
  });

  test("a command without placeholders is left alone", () => {
    expect(rewrite("echo ${HOME} $secret")).toBeUndefined();
  });
});
