import { describe, expect, test } from "bun:test";
import { type AskQuestion, askQuestions, askResult, parseReplies } from "./ask.ts";

const db: AskQuestion = {
  id: "db",
  question: "Database?",
  options: [{ label: "SQLite" }, { label: "Postgres" }],
};
const os: AskQuestion = {
  id: "os",
  question: "Platforms?",
  options: [{ label: "linux" }, { label: "macos" }, { label: "windows" }],
  multi: true,
};
const extras: AskQuestion = {
  id: "extras",
  question: "Extras?",
  options: [{ label: "docs" }, { label: "bench" }],
  multi: true,
};

describe("askResult", () => {
  test("a single select reads like omp's own", () => {
    expect(askResult([db], [{ id: "db", selected: ["Postgres"] }])).toEqual({
      content: [{ type: "text", text: "User selected: Postgres" }],
      details: {
        question: "Database?",
        options: ["SQLite", "Postgres"],
        multi: false,
        selectedOptions: ["Postgres"],
      },
    });
  });

  test("a multi-line custom answer is indented under its line", () => {
    const result = askResult([db], [{ id: "db", selected: [], custom: "MariaDB\nwith Galera" }]);
    expect(result.content[0].text).toBe("User provided custom input:\n  MariaDB\n  with Galera");
    expect(result.details).toMatchObject({ selectedOptions: [], customInput: "MariaDB\nwith Galera" });
  });

  test("several questions: a multi with a custom answer, an empty multi and a single", () => {
    const result = askResult(
      [os, extras, db],
      [
        { id: "os", selected: ["linux", "windows"], custom: "and BSD" },
        { id: "extras", selected: [] },
        { id: "db", selected: ["SQLite"] },
      ],
    );
    expect(result.content[0].text).toBe(
      'User answers:\nos: [linux, windows] + "and BSD"\nextras: []\ndb: SQLite',
    );
    expect(result.details).toEqual({
      results: [
        {
          id: "os",
          question: "Platforms?",
          options: ["linux", "macos", "windows"],
          multi: true,
          selectedOptions: ["linux", "windows"],
          customInput: "and BSD",
        },
        {
          id: "extras",
          question: "Extras?",
          options: ["docs", "bench"],
          multi: true,
          selectedOptions: [],
        },
        {
          id: "db",
          question: "Database?",
          options: ["SQLite", "Postgres"],
          multi: false,
          selectedOptions: ["SQLite"],
        },
      ],
    });
  });
});

describe("parseReplies", () => {
  test("the line ci ask prints", () => {
    expect(parseReplies('{"replies":[{"id":"db","selected":["Postgres"]}]}\n')).toEqual([
      { id: "db", selected: ["Postgres"] },
    ]);
  });

  test("malformed output is no answer", () => {
    expect(parseReplies("")).toBeUndefined();
    expect(parseReplies("closed\n")).toBeUndefined();
    expect(parseReplies('{"replies":[{"id":"db"}]}')).toBeUndefined();
    expect(parseReplies('{"replies":{"id":"db","selected":[]}}')).toBeUndefined();
  });
});

describe("askQuestions", () => {
  test("a recommended that is no index is dropped, the rest kept", () => {
    const questions = askQuestions({ questions: [{ ...db, recommended: 1.5, header: "Storage" }] });
    expect(questions).toEqual([{ ...db, header: "Storage" }]);
    expect(askQuestions({ questions: [{ ...db, recommended: 1 }] })).toEqual([{ ...db, recommended: 1 }]);
  });

  test("anything but a list of questions with options is none", () => {
    expect(askQuestions({ questions: [] })).toBeUndefined();
    expect(askQuestions({ questions: [{ id: "db", question: "?" }] })).toBeUndefined();
    expect(askQuestions({})).toBeUndefined();
  });
});
