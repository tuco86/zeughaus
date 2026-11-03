import { describe, expect, test } from "bun:test";
import { linkTickets, parseTicketLink, showSequence } from "./tickets.ts";

describe("linkTickets", () => {
  test("own and other repositories' tickets become links", () => {
    expect(linkTickets("see #12 and #weida-3.", "zeughaus")).toBe(
      "see [#12](zeughaus-ticket:zeughaus/12) and [#weida-3](zeughaus-ticket:weida/3).",
    );
    expect(linkTickets("#assistant-rs-12", undefined)).toBe(
      "[#assistant-rs-12](zeughaus-ticket:assistant-rs/12)",
    );
  });

  test("code, links, anchors, words, doubled hashes and ticket 0 stay as they are", () => {
    expect(linkTickets("`#12` [#12](https://x) https://x/#12 a#12 ##2 #0", "zeughaus")).toBe(
      undefined,
    );
  });

  test("fenced code is left alone", () => {
    expect(linkTickets("```\n#12\n```\n#4", "zeughaus")).toBe(
      "```\n#12\n```\n[#4](zeughaus-ticket:zeughaus/4)",
    );
  });

  test("without a repository at hand only named ones link", () => {
    expect(linkTickets("#12 #weida-3", undefined)).toBe("#12 [#weida-3](zeughaus-ticket:weida/3)");
  });
});

describe("parseTicketLink", () => {
  test("the scheme in any case names a ticket", () => {
    expect(parseTicketLink("zeughaus-ticket:weida/12")).toEqual({ repo: "weida", number: 12 });
    expect(parseTicketLink("ZEUGHAUS-TICKET:weida/12")).toEqual({ repo: "weida", number: 12 });
  });

  test("anything else names none", () => {
    for (const url of [
      "zeughaus-ticket:weida/0",
      "zeughaus-ticket:../x/1",
      "zeughaus-ticket:weida/12/x",
      "https://x",
    ]) {
      expect(parseTicketLink(url)).toBe(undefined);
    }
  });
});

test("showSequence sets zeughaus_show to the request", () => {
  const json = '{"v":1,"repo":"weida","number":12,"at":1000}';
  expect(showSequence({ repo: "weida", number: 12 }, 1000)).toBe(
    `\x1b]1337;SetUserVar=zeughaus_show=${Buffer.from(json).toString("base64")}\x07`,
  );
});
