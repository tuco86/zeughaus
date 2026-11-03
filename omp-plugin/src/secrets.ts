// Secret placeholders in bash commands: `${secret:hannes/omp/<doc...>/<FIELD>}` is replaced by
// the field's value when the command runs, read by `get-secret` inside a subshell, so the value
// never enters the command text omp shows and stores, nor omp's persistent shell. Only the
// owner's omp documents resolve here; a zeughaus shell already has its project's zeughaus
// secrets in its environment.

export const PLACEHOLDER = /\$\{secret:([A-Za-z0-9._\/-]+)\/([A-Za-z_][A-Za-z0-9_]*)\}/g;

/** The document every placeholder must be in or below. */
const ROOT = "hannes/omp";

/**
 * The command with its placeholders resolved at run time, a refusal, or `undefined` when it has
 * none. Each distinct `(path, FIELD)` becomes one variable `ZHS_<FIELD>` (`_2`, `_3`, ... for the
 * same FIELD from another document). The command runs in a subshell, so a `cd` or an export in
 * it does not persist.
 */
export function rewrite(command: string): { command: string } | { refused: string } | undefined {
  const matches = [...command.matchAll(new RegExp(PLACEHOLDER.source, "g"))];
  if (matches.length === 0) return undefined;
  const vars = new Map<string, string>(); // "path/FIELD" -> variable
  const taken = new Set<string>();
  const refs: string[] = [];
  for (const [, path, field] of matches) {
    if (path !== ROOT && !path.startsWith(`${ROOT}/`)) {
      return { refused: `only secrets under ${ROOT} resolve in omp sessions: ${path}/${field}` };
    }
    const key = `${path}/${field}`;
    if (vars.has(key)) continue;
    let name = `ZHS_${field}`;
    for (let n = 2; taken.has(name); n++) name = `ZHS_${field}_${n}`;
    taken.add(name);
    vars.set(key, name);
    refs.push(`'${name}=secret/${key}'`);
  }
  const body = command.replace(
    new RegExp(PLACEHOLDER.source, "g"),
    (_whole, path: string, field: string) => `\${${vars.get(`${path}/${field}`)}}`,
  );
  // `eval "$(get-secret ...)"` would hide a failed read: eval of nothing succeeds.
  return {
    command: `( ZHS__=$(get-secret --export ${refs.join(" ")}) || exit 1; eval "$ZHS__"; unset ZHS__\n${body}\n)`,
  };
}
