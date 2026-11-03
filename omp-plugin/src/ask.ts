// omp's `ask`, forwarded to zeughaus: the questions as the CI's `ask_post` carries them, and the
// tool result omp's own ask gives for the answer an editor sends, so that an answer from the
// editor reads to the agent like one from the terminal.

export interface AskOption {
  label: string;
  description?: string;
  preview?: string;
}

export interface AskQuestion {
  id: string;
  question: string;
  options: AskOption[];
  multi?: boolean;
  recommended?: number;
  header?: string;
}

export interface AskReply {
  id: string;
  selected: string[];
  custom?: string;
}

const isObject = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

function isOption(value: unknown): value is AskOption {
  return isObject(value) && typeof value.label === "string";
}

function isQuestion(value: unknown): value is AskQuestion {
  return (
    isObject(value) &&
    typeof value.id === "string" &&
    typeof value.question === "string" &&
    Array.isArray(value.options) &&
    value.options.every(isOption)
  );
}

/**
 * The questions of an `ask` call as given, without a `recommended` that is no index; `undefined`
 * when they are not a non-empty list of questions with options.
 */
export function askQuestions(params: Record<string, unknown>): AskQuestion[] | undefined {
  const questions = params.questions;
  if (!Array.isArray(questions) || questions.length === 0 || !questions.every(isQuestion)) {
    return undefined;
  }
  return questions.map((q) => {
    const { recommended, ...rest } = q;
    const index = typeof recommended === "number" && Number.isInteger(recommended) && recommended >= 0;
    return index ? q : rest;
  });
}

interface Answered {
  id: string;
  question: string;
  options: string[];
  multi: boolean;
  selectedOptions: string[];
  customInput?: string;
}

function answered(question: AskQuestion, reply: AskReply | undefined): Answered {
  return {
    id: question.id,
    question: question.question,
    options: question.options.map((o) => o.label),
    multi: question.multi ?? false,
    selectedOptions: reply?.selected ?? [],
    ...(reply?.custom !== undefined ? { customInput: reply.custom } : {}),
  };
}

/** One question's text, as omp's ask writes it. */
function singleText(a: Answered): string {
  const lines: string[] = [];
  if (a.selectedOptions.length > 0) {
    lines.push(`User selected: ${a.multi ? a.selectedOptions.join(", ") : a.selectedOptions[0]}`);
  }
  if (a.customInput !== undefined) {
    lines.push(
      a.customInput.includes("\n")
        ? `User provided custom input:\n${a.customInput
            .split("\n")
            .map((line) => `  ${line}`)
            .join("\n")}`
        : `User provided custom input: ${a.customInput}`,
    );
  }
  if (lines.length > 0) return lines.join("\n");
  return a.multi ? "User did not select any options" : "User cancelled the selection";
}

/** One line of a several-question answer, as omp's ask writes it. */
function answerLine(a: Answered): string {
  const custom = a.customInput === undefined ? undefined : `"${a.customInput}"`;
  if (a.selectedOptions.length === 0) {
    if (custom !== undefined) return `${a.id}: ${custom}`;
    return a.multi ? `${a.id}: []` : `${a.id}: (cancelled)`;
  }
  const chosen = a.multi ? `[${a.selectedOptions.join(", ")}]` : a.selectedOptions[0];
  return `${a.id}: ${custom === undefined ? chosen : `${chosen} + ${custom}`}`;
}

/** The result omp's own ask gives for these replies. */
export function askResult(
  questions: AskQuestion[],
  replies: AskReply[],
): { content: [{ type: "text"; text: string }]; details: unknown } {
  const all = questions.map((q) => answered(q, replies.find((r) => r.id === q.id)));
  const [only] = all;
  if (all.length === 1 && only) {
    const { id: _id, ...details } = only;
    return { content: [{ type: "text", text: singleText(only) }], details };
  }
  return {
    content: [{ type: "text", text: `User answers:\n${all.map(answerLine).join("\n")}` }],
    details: { results: all },
  };
}
