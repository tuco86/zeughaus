// What a bash command line runs: which of its commands push this checkout, and where its
// `git commit`s are, to add trailers to them.

interface Heredoc {
  tag: string;
  indent: boolean;
}

/** A word of a command with the offset just past it in the line. */
interface Word {
  text: string;
  end: number;
}

/**
 * The simple commands of a shell line, as lists of words with quotes and escapes resolved, each
 * with the offset in `line` where it ends. It is no shell: it knows quoting, comments,
 * here-documents and the separators `; & | ( )` and newline, which is what it takes to tell what a
 * line runs, as opposed to what it only mentions in a commit message or an echo.
 */
function commandWords(line: string): Word[][] {
  const commands: Word[][] = [];
  const heredocs: Heredoc[] = [];
  let words: Word[] = [];
  let word: string | undefined;
  let heredoc: Heredoc | undefined; // `<<` seen: the next word is its delimiter
  let i = 0;

  // Every caller ends a word while `i` points at the separator after it, or at the line's end.
  const endWord = (): void => {
    if (word === undefined) return;
    if (heredoc) {
      heredocs.push({ tag: word, indent: heredoc.indent });
      heredoc = undefined;
    } else {
      words.push({ text: word, end: i });
    }
    word = undefined;
  };
  const endCommand = (): void => {
    endWord();
    if (words.length > 0) commands.push(words);
    words = [];
  };

  while (i < line.length) {
    const ch = line.charAt(i);
    if (ch === "'") {
      const close = line.indexOf("'", i + 1);
      const end = close === -1 ? line.length : close;
      word = (word ?? "") + line.slice(i + 1, end);
      i = end + 1;
    } else if (ch === '"') {
      let text = "";
      i += 1;
      while (i < line.length && line.charAt(i) !== '"') {
        if (
          line.charAt(i) === "\\" &&
          i + 1 < line.length &&
          '"\\$`\n'.includes(line.charAt(i + 1))
        ) {
          i += 1;
        }
        text += line.charAt(i);
        i += 1;
      }
      word = (word ?? "") + text;
      i += 1;
    } else if (ch === "\\") {
      if (line.charAt(i + 1) !== "\n") word = (word ?? "") + line.charAt(i + 1);
      i += 2;
    } else if (ch === "\n") {
      endCommand();
      i += 1;
      for (const h of heredocs) {
        while (i < line.length) {
          const end = line.indexOf("\n", i);
          const text = line.slice(i, end === -1 ? line.length : end);
          i = end === -1 ? line.length : end + 1;
          if ((h.indent ? text.replace(/^\t+/, "") : text) === h.tag) break;
        }
      }
      heredocs.length = 0;
    } else if (ch === " " || ch === "\t" || ch === "\r") {
      endWord();
      i += 1;
    } else if (ch === "#" && word === undefined) {
      const end = line.indexOf("\n", i);
      i = end === -1 ? line.length : end;
    } else if (ch === "<" && line.startsWith("<<", i) && line.charAt(i + 2) !== "<") {
      endWord();
      heredoc = { tag: "", indent: line.charAt(i + 2) === "-" };
      i += heredoc.indent ? 3 : 2;
    } else if (
      ch === "&" &&
      (word?.endsWith(">") || word?.endsWith("<") || line.charAt(i + 1) === ">")
    ) {
      word = (word ?? "") + ch; // `2>&1`, `&>file`: a redirection, not a separator
      i += 1;
    } else if (";|&()`".includes(ch)) {
      endCommand();
      i += 1;
    } else {
      word = (word ?? "") + ch;
      i += 1;
    }
  }
  i = Math.min(i, line.length);
  endCommand();
  return commands;
}

// Words that may stand before a command without being it.
const PREFIX =
  /^(?:if|then|else|elif|do|while|until|!|\{|\}|time|command|exec|nohup|sudo|env|builtin)$/;
const ASSIGNMENT = /^[A-Za-z_][A-Za-z0-9_]*=/;
const REDIRECT = /^(?:\d*|&)(?:>>?|<)/;
const REDIRECT_ONLY = /^(?:\d*|&)(?:>>?|<)$/;
const TAKES_VALUE = /^(?:-o|--push-option|--repo|--receive-pack|--exec)$/;

/** The `-C` directories and the index of the subcommand word of a command that is `git <name>`. */
function gitSubcommand(words: string[], name: string): { dirs: string[]; at: number } | undefined {
  let k = 0;
  while (k < words.length) {
    const w = words[k] ?? "";
    if (ASSIGNMENT.test(w) || PREFIX.test(w)) k += 1;
    else if (w === "timeout" && k + 1 < words.length) k += 2;
    else break;
  }
  const program = words[k];
  if (program !== "git" && !program?.endsWith("/git")) return undefined;
  k += 1;
  const dirs: string[] = [];
  while (k < words.length && words[k] !== name) {
    const w = words[k] ?? "";
    if (w === "-C" && k + 1 < words.length) {
      dirs.push(words[k + 1] ?? "");
      k += 2;
    } else if (w === "-c" && k + 1 < words.length) {
      k += 2;
    } else if (w.startsWith("-")) {
      k += 1;
    } else {
      return undefined; // another subcommand
    }
  }
  if (words[k] !== name) return undefined;
  return { dirs, at: k };
}

/** The `-C` directories and the arguments of a command that is `git push`. */
function gitPush(words: string[]): { dirs: string[]; args: string[] } | undefined {
  const push = gitSubcommand(words, "push");
  if (!push) return undefined;
  return { dirs: push.dirs, args: words.slice(push.at + 1) };
}

/**
 * Whether the arguments of a `git push` can start a pipeline of the checked-out commit. Not so
 * for a dry run, a deletion, and a push of tags only (`--tags` without a refspec).
 */
function pushesBranch(args: string[]): boolean {
  const flags: string[] = [];
  const positional: string[] = [];
  let rest = false; // after `--`
  for (let i = 0; i < args.length; i++) {
    const a = args[i] ?? "";
    if (!rest && REDIRECT.test(a)) {
      if (REDIRECT_ONLY.test(a)) i += 1; // its target is the next word
    } else if (!rest && a === "--") {
      rest = true;
    } else if (!rest && a.startsWith("-")) {
      flags.push(a);
      if (TAKES_VALUE.test(a)) i += 1;
    } else {
      positional.push(a);
    }
  }
  const has = (pattern: RegExp): boolean => flags.some((f) => pattern.test(f));
  if (has(/^--dry-run$/) || has(/^-[A-Za-z]*n[A-Za-z]*$/)) return false;
  if (has(/^--delete$/) || has(/^-[A-Za-z]*d[A-Za-z]*$/)) return false;
  if (has(/^--tags$/) && positional.length <= 1) return false;
  return true;
}

/**
 * For every `git push` of a bash command that can start a pipeline, the directories it runs in
 * relative to the command's own: the `cd` targets before it, then its `-C` options.
 */
export function branchPushes(command: string): string[][] {
  const found: string[][] = [];
  const cds: string[] = [];
  for (const ws of commandWords(command)) {
    const words = ws.map((w) => w.text);
    if (words[0] === "cd" && words[1] !== undefined) {
      cds.push(words[1]);
      continue;
    }
    const push = gitPush(words);
    if (push && pushesBranch(push.args)) found.push([...cds, ...push.dirs]);
  }
  return found;
}

/**
 * `command` with `--trailer '<t>'` for each of `trailers` right after the `commit` word of every
 * `git commit` it runs; `undefined` when there are no trailers or no such command. A trailer must
 * not contain `'`.
 */
export function withTrailers(command: string, trailers: string[]): string | undefined {
  if (trailers.length === 0) return undefined;
  const at: number[] = [];
  for (const ws of commandWords(command)) {
    const commit = gitSubcommand(
      ws.map((w) => w.text),
      "commit",
    );
    const word = commit && ws[commit.at];
    if (word) at.push(word.end);
  }
  if (at.length === 0) return undefined;
  const options = trailers.map((t) => ` --trailer '${t}'`).join("");
  let out = command;
  for (const end of at.sort((a, b) => b - a)) {
    out = out.slice(0, end) + options + out.slice(end);
  }
  return out;
}
