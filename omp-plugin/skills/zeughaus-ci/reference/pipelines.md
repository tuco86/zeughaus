# Shaping a pipeline for zeughaus-ci

zeughaus's own `.zeughaus-ci/` is the worked example: one Rust workspace, built natively on four platforms, checked there and published from a fifth place. The principles are not about Rust. Each says what to do and why this CI makes it right. The header keys and the job environment are in `skill://zeughaus-ci/reference/jobs.md`.

## The example: zeughaus

| file | role |
|---|---|
| `channels.toml` | `dev` on `push main`; `nightly` on `cron *` and `release` on `tag v*`, both `clean` |
| `trixie.Containerfile` | The image of the Linux jobs: `rust:1.99.1-trixie`, the libraries the workspace links, clippy, rustfmt and the wasm target |
| `build-<p>.sh`, `build-windows-amd64.ps1` | The release archive of platform `<p>`, written to `$CI_OUTPUT` |
| `check-<p>.sh`, `check-windows-amd64.ps1` | The gate of `<p>`: clippy, the tests (the unit tier on a push or manual run, both tiers nightly and on a tag) and the doctests. `check-linux-amd64` also runs `fmt` |
| `publish.sh` | Needs all eight; bundles, signs and publishes the archives. The only job with release secrets |
| `build.sh`, `weida.sh`, `windows.ps1` | Helpers without a header; the jobs source them |

`<p>` is `linux-amd64`, `linux-arm64`, `darwin-arm64` or `windows-amd64`. The headers of one pair, `build-linux-amd64.sh` and `check-linux-amd64.sh`:

```sh
#!/bin/sh
# /// ci
# on = ["push main", "tag v*", "cron 0 3 * * *"]
# platform = "linux-amd64"
# image = "trixie"
# secrets = ["FORGEJO_READ_TOKEN"]
# when_busy = "freeze"
# timeout_minutes = 120
# ///
```

```sh
#!/bin/sh
# /// ci
# needs = ["build-linux-amd64"]
# platform = "linux-amd64"
# image = "trixie"
# secrets = ["FORGEJO_READ_TOKEN"]
# when_busy = "freeze"
# timeout_minutes = 120
# ///
```

The build is selected by its `on`. The check has no `on`: its `needs` select it with the build, and it never runs alone from a push. Both name the platform and the image: a container built from `.zeughaus-ci/trixie.Containerfile` on the runner that ci.toml's `[platforms]` gives `linux-amd64` (tuco-pc). `when_busy = "freeze"` pauses them while that desktop is busy. `FORGEJO_READ_TOKEN` is for fetching weida's `main` (principle 7).

## Principles

### 1. One job per role and platform

- `build-<p>` builds natively on `<p>` and writes the platform's artifacts to `$CI_OUTPUT`; `check-<p>` lints and tests there. One pair per platform; nothing is cross-compiled.
- Why: these names are what `run_jobs` takes and what an agent runs to debug `<p>`. Each pair has its own place and workspace, so pairs of different platforms do not wait for each other and a red platform does not stop the others.

### 2. Place with `platform`; reach Windows through a runner's host

- Say what a job builds for with `platform = "<p>"`; ci.toml's `[platforms]` says who builds it. Add `image = "<name>"` for a container on that runner.
- Why: moving a platform to another machine is one edit of ci.toml, not of every job file. The job sees `CI_PLATFORM`, so helpers name archives and directories by it (`build.sh` does).
- A Windows job that agents must reach with `run_jobs` runs on a Windows runner's host: `machine = "<runner>"` in a `.ps1` file. zeughaus: `machine = "wohnzimmer"`.
- Why: a manual run is refused on a VM, whose workspace would be copied from one bundle. zeughaus names the host explicitly because its `windows-amd64` platform stays on the `win11` VM, which another repository's job needs.

### 3. Warm state lives in ignored directories of the workspace

- Keep cargo's home and target directory, tool downloads and sibling clones in one gitignored directory of the workspace. zeughaus: `.ci-cache/`; `cargo_env` in `build.sh` points `CARGO_HOME` and `CARGO_TARGET_DIR` into it, and `.gitignore` lists it, as it does `target/`.
- Why: the jobs of one repository at one place share one workspace, pipeline jobs and manual runs alike. Each checkout resets tracked files and runs `git clean -ffd`: untracked files go, ignored files stay. An ignored directory is the warm cache; anything else is gone before the next job.
- A workspace is a cache, not state. A `clean` channel wipes it (`git clean -ffdx`) at the first use of each place in a pipeline, and a runner deletes the least recently used workspaces beyond its budget. Write every job so that it also works from an empty workspace.

### 4. Empty what you publish

- A job that collects files for an artifact or a package out of an ignored directory empties that directory first (`rm -rf dist`). zeughaus's `stage_dir` removes the stage directory before it fills it.
- Why: the workspace keeps last run's files. A version bump would otherwise publish the old packages beside the new ones. `$CI_OUTPUT` itself is a fresh directory per run.

### 5. Order is `needs`, never file names

- Say what must come first with `needs`. zeughaus: `check-<p>` needs `build-<p>`; `publish` needs all eight.
- Why: without `needs`, jobs at different places run in parallel and jobs at one place queue in the folder's order (topological, ties by name), which no key states. A red job skips only its dependents (`skipped: <need> failed (#<n>)`). `needs` also stages the needed job's `$CI_OUTPUT` as `$CI_INPUTS/<need>`, across runners: `publish` takes the archives of four platforms that way. A need that left no files stages no directory.
- Build before check is zeughaus's choice, not artifact sharing. The archive is ready early, so the CI can move it to the publishing runner while the checks run, and a red build skips its platform's check. The check runs in the workspace the build left: cargo's home, the downloaded crates and the weida checkout are warm. It does not read the build's output, and it compiles other profiles (`--release` against dev and test). The other order, check first, skips the build when the gate is red; either is valid.

### 6. Secrets only in publish jobs

- Give `secrets` to the jobs that publish or deploy, and keep build and check jobs free of them where you can. Grant each secret in ci.toml to the events that need it. zeughaus grants the signing key and the GitHub token to `push main`, `cron *` and `tag v*`.
- Why: a manual run gives no job any secret, so a job that needs one to build cannot be reached by `run_jobs`. A failed job that holds secrets also keeps its cause line out of the forge's commit status.
- zeughaus's build and check jobs hold only `FORGEJO_READ_TOKEN`, to fetch weida's `main`. `weida.sh` skips that fetch when a manual run brings `weida` in `CI_SIBLINGS`, so they still run there.

### 7. Sibling path dependencies

- A job that builds against `../<dir>` uses `$CI_SIBLINGS/<dir>` when `CI_SIBLINGS` is set and fetches the dependency otherwise. Either way the checkout ends in one stable path of the cache, and `../<dir>` links to it: `weida.sh` for the Unix jobs, a junction in `windows.ps1`.
- Why: a manual run uploads the sibling's working tree to `$CI_SIBLINGS`, and `run_jobs` should test that tree, not the sibling's `main`. The stable path keeps cargo's fingerprints from seeing the sources move between trees of different mtimes, which would rebuild.
- The fetch token travels in the environment (`GIT_CONFIG_*`), never in argv, and is unset afterwards.

### 8. Pin the toolchain

- `rust-toolchain.toml` at the workspace root and the `FROM` tag of the image name the same version. Change both in one commit.

```toml
[toolchain]
channel = "1.99.1"
components = ["clippy", "rustfmt"]
targets = ["wasm32-unknown-unknown"]
```

```
FROM docker.io/library/rust:1.99.1-trixie
```

- Why: all places build with one compiler, so a result on one platform means the same on the others and no place rebuilds because it drifted. A bump rebuilds each place's cache once.

### 9. Time the steps

- Print how long each step took: `== <cmd>: N s`, or `== <cmd> failed after N s`. zeughaus: `step` in `build.sh`, `Run` in `windows.ps1`.
- Why: `ci_log` then shows where a job's time went without a profiler, and `ci_timing` with `job` reads the time the transcript keeps for every chunk of output to name the longest pauses.

### 10. channels.toml

- `dev` on `push main`; `nightly` on `cron *` and `release` on `tag v*`, both with `clean = true`.
- Why: a channel is a stream of pipelines (`ci_pipelines` takes one). `clean` starts every place's workspace from nothing in that channel's pipelines, so what nightly and release publish is built from empty workspaces, not from warm caches, and a cache gone bad shows within a day.
- A channel starts nothing; a job's `on` does. zeughaus's jobs name only `push main`, `tag v*` and one cron, so a push to any other branch starts no pipeline, and no channel is named after a branch.

### 11. `when_busy = "freeze"` for container jobs on a desktop that someone uses

- A runner that someone works on has a `[busy]` table in its enlist.toml (zeughaus: tuco-pc, measured by GPU load). Give its container jobs `when_busy = "freeze"`.
- Why: a running `freeze` job is paused while the runner is busy and resumed when it is free again; a `wait` job (the default) is only held back before it starts. A host job cannot be paused: `freeze` on a host placement is refused when the pipeline is set up. Apple's `container` cannot pause, so there the job runs on.

### 12. Unit tests on a push, every tier nightly and on a release

- A check job runs the unit tier on `push` and `manual`, and the integration and end-to-end tiers too on `cron` and `tag`: it chooses by `CI_EVENT`. zeughaus: `check_platform` in `build.sh` and `check-windows-amd64.ps1` run `cargo nextest run --profile default` or `--profile ci`; the doctests run on every event.
- Why: agents run only the unit tier of the touched crates before a push, so the push pipeline is where that tier runs on every platform, fast enough to answer the push. The I/O tiers are slower and depend on the machine; they run once a day in `clean` workspaces and before a release is published.
- A manual run follows a push, so `run_jobs` reproduces the push pipeline. For an integration failure, edit the job's test command temporarily (skill section 4).

## Review checklist

Walk a repository's `.zeughaus-ci/` with these; every hit is a finding.

- `cache` or `context` header keys are left: they do nothing, and `zeughaus-ci check` warns.
- A job collects files out of an ignored directory (`dist/`, a stage directory) without emptying it first.
- Order depends on file names or on the position in the folder instead of `needs`.
- `secrets` sit on a build or check job where they are avoidable: `run_jobs` gives it none, so it breaks there unless it has a fallback.
- A Windows job that agents need runs on a VM (`machine = "<vm>"`, or a platform that maps to one): manual runs refuse it. Move it to a Windows runner's host.
- The toolchain is unpinned, or the image's tag floats or differs from `rust-toolchain.toml`.
- A cache lives in a tracked or non-ignored path: `git clean -ffd` deletes it before every job, or it gets committed.
- A job has no `timeout_minutes` and can hang: it holds its place's workspace for the default 240 minutes while every later job at that place waits.
- A job is not part of a `build-<p>` / `check-<p>` pair where agents will look for one.
- A check job runs the integration or end-to-end tier on a push, or no `cron` or `tag` event ever runs it.
