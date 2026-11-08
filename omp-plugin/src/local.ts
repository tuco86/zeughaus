// Where omp keeps a session's `local://` files, mirroring omp's `resolveLocalRoot`
// (internal-urls/local-protocol.ts): `<artifacts>/local`, or `<tmpdir>/omp-local/<session id>`
// on Windows when that path is long (>= 180 chars) and without an artifacts directory.
// Pure; the caller passes `tmpdir` and an `exists` predicate.

import { join } from "node:path";

/** omp's `safeSessionId`: the session id with every character outside `[a-zA-Z0-9_.-]` as `_`; `session` when there is none, or it is empty, `.` or `..`. */
export function safeSessionId(id: string | undefined): string {
  const safe = (id ?? "session").replace(/[^a-zA-Z0-9_.-]/g, "_");
  return safe === "" || safe === "." || safe === ".." ? "session" : safe;
}

/** The directories omp may keep this session's `local://` files in, the artifacts one first. */
export function localRoots(
  artifacts: string | undefined | null,
  sessionId: string | undefined,
  tmpdir: string,
): string[] {
  const temp = join(tmpdir, "omp-local", safeSessionId(sessionId));
  return artifacts ? [join(artifacts, "local"), temp] : [temp];
}

/** `name` in the first root that has it; else in the first root, a path that does not exist. */
export function localFile(name: string, roots: string[], exists: (path: string) => boolean): string {
  const paths = roots.map((root) => join(root, name));
  return paths.find(exists) ?? paths[0];
}
