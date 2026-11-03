import { describe, expect, test } from "bun:test";
import {
  MAX_CI_REQUEST_BYTES,
  ciUrl,
  decodeReply,
  encodeRequest,
  pickCiUrl,
  stateDir,
  withoutCredentials,
} from "./protocol.ts";

const FP = `sha256:${"0".repeat(63)}1`;
const encode = (text: string): Uint8Array => new TextEncoder().encode(text);

describe("ciUrl", () => {
  test("a pinned URL becomes its /ci path", () => {
    expect(ciUrl(`weida://${FP}@10.8.0.1:7444/`)).toBe(`weida://${FP}@10.8.0.1:7444/ci`);
    expect(ciUrl(`weida://${FP}@10.8.0.1:7444`)).toBe(`weida://${FP}@10.8.0.1:7444/ci`);
  });

  test("a URL without a fingerprint is refused as unpinned", () => {
    expect(() => ciUrl("weida://10.8.0.1:7444/")).toThrow(
      "the CI URL weida://10.8.0.1:7444/ names no fingerprint (weida://sha256:<fp>@host:port/): a CI that is not pinned could be anyone",
    );
  });

  test("anything else is not valid", () => {
    for (const url of ["not a url", "https://ci/", "weida://sha256:12@host:1/", "weida://user@host:1/"]) {
      expect(() => ciUrl(url)).toThrow(`the CI URL ${url} is not valid`);
    }
  });
});

describe("withoutCredentials", () => {
  test("the user and password of the authority go", () => {
    expect(withoutCredentials("https://user:tok@host/x")).toBe("https://host/x");
    expect(withoutCredentials("ssh://git@host:22/o/r.git")).toBe("ssh://host:22/o/r.git");
  });

  test("an scp-style remote stays as it is", () => {
    expect(withoutCredentials("git@host:o/r.git")).toBe("git@host:o/r.git");
  });
});

describe("stateDir", () => {
  test("ZEUGHAUS_STATE_DIR wins", () => {
    const env = { ZEUGHAUS_STATE_DIR: "/srv/zeughaus", XDG_STATE_HOME: "/home/u/.local/state", HOME: "/home/u" };
    expect(stateDir(env, "linux")).toBe("/srv/zeughaus");
  });

  test("an empty value is unset, not a path", () => {
    expect(stateDir({ ZEUGHAUS_STATE_DIR: "", HOME: "/home/u" }, "darwin")).toBe("/home/u/.local/state/zeughaus");
  });

  test("XDG_STATE_HOME before HOME", () => {
    expect(stateDir({ XDG_STATE_HOME: "/home/u/state", HOME: "/home/u" }, "linux")).toBe("/home/u/state/zeughaus");
  });

  test("LOCALAPPDATA on Windows, never HOME", () => {
    expect(stateDir({ LOCALAPPDATA: "C:\\Users\\u\\AppData\\Local", HOME: "/home/u" }, "win32")).toBe(
      "C:\\Users\\u\\AppData\\Local\\zeughaus\\state",
    );
    expect(stateDir({ HOME: "/home/u" }, "win32")).toBe(".zeughaus-state");
  });

  test("no home at all is a relative directory", () => {
    expect(stateDir({}, "linux")).toBe(".zeughaus-state");
  });
});

describe("pickCiUrl", () => {
  const editor = (text: string | null) => ({ path: "/s/zeughaus.toml", text });
  const enlist = (text: string | null) => ({ path: "/s/enlist.toml", text });

  test("the environment, then the editor's first CI, then the runner's", () => {
    const file = editor('remotes = ["weida://x/"]\nci = ["weida://a/", "weida://b/"]\n');
    expect(pickCiUrl("weida://env/", file, enlist('ci = "weida://e/"'))).toBe("weida://env/");
    expect(pickCiUrl("", file, enlist('ci = "weida://e/"'))).toBe("weida://a/");
    expect(pickCiUrl(undefined, editor("ci = []\n"), enlist('ci = "weida://e/"\nname = "x"'))).toBe("weida://e/");
    expect(pickCiUrl(undefined, editor(null), enlist('ci = "weida://e/"'))).toBe("weida://e/");
  });

  test("no CI anywhere names the editor's file", () => {
    expect(() => pickCiUrl(undefined, editor("theme = 'x'\n"), enlist(null))).toThrow(
      "no CI configured: add ci = [..] to /s/zeughaus.toml",
    );
  });

  test("a file that does not parse is named", () => {
    expect(() => pickCiUrl(undefined, editor("ci = [\n"), enlist(null))).toThrow("/s/zeughaus.toml: ");
    expect(() => pickCiUrl(undefined, editor("ci = 3\n"), enlist(null))).toThrow("/s/zeughaus.toml: ");
  });
});

describe("encoding", () => {
  test("a request larger than the CI reads is refused", () => {
    const plan = "x".repeat(MAX_CI_REQUEST_BYTES);
    expect(() => encodeRequest({ kind: "doc", repo: plan, number: 1 })).toThrow(
      /^the request is \d+ bytes, more than the CI reads \(540672\)$/,
    );
  });

  test("a reply decodes by its kind; the CI's refusal is the error", () => {
    expect(decodeReply(encode('{"kind":"doc_created","number":4}'))).toEqual({ kind: "doc_created", number: 4 });
    expect(() => decodeReply(encode('{"kind":"error","message":"no document #3 in demo"}'))).toThrow(
      "no document #3 in demo",
    );
  });

  test("anything but an object with a kind is malformed", () => {
    for (const text of ["", "null", "[]", "3", '{"number":4}', "{"]) {
      expect(() => decodeReply(encode(text))).toThrow("ci: malformed reply");
    }
  });
});
