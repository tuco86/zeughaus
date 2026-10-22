---
name: zeughaus-ci
description: "Repo has .zeughaus-ci/: debug a job or platform with run_jobs, CI state, after a push, write or review CI jobs."
---

# zeughaus-ci

zeughaus-ci is the user's own CI. Its runners are the machines they own, one per platform. A repository's jobs are scripts in its `.zeughaus-ci/` folder. You reach the CI through the tools of the MCP server `zeughaus`: `ci_repo`, `ci_status`, `ci_pipelines`, `ci_pipeline`, `ci_log`, `ci_timing`, `ci_wait`, `ci_rerun`, `ci_cancel`, `run_jobs`, `docs`, `doc_read`, `proposal_write`, `plan_done`.

## 1. Orient

- Read the `<zeughaus-ci>` block of the system prompt first. It is the state at session start: which repository of the CI this checkout is, every job with the runner and place it runs at, whether HEAD and the default branch were green. It does not change during the session.
- Call `ci_repo` for the state now: the pipeline of HEAD, the channels, the jobs, the runners, the platforms. `ci_status` shows the whole CI: every channel, every runner online or busy, the queue.
- In the checkout, leave `repo` out of the other `ci_*` tools. It defaults to the repository of the working directory's checkout.
- Job names are the file stems of `.zeughaus-ci/`: `build-linux-amd64.sh` is the job `build-linux-amd64`. `run_jobs`, `ci_log` and `needs` use these names.
- Read a job row as: name, runner, where on it (`host`, `image:<x>` or `machine:<vm>`), then `on <patterns>` or `needs <jobs>`, then `channels <names>`, `secrets <names> (none in run_jobs)` and `run_jobs cannot run it` when they apply. A job the CI cannot place says `cannot be placed: <why>` in place of the runner.
- The job rows are read at the commit the `jobs at <sha7>` line names: HEAD once the CI has fetched it, else the default branch's head. Your uncommitted edits to `.zeughaus-ci/` are not in them; `run_jobs` reads those.
- `HEAD <sha7>: no pipeline` means the commit is not pushed, or no job's `on` selects it.
- An origin the CI does not know (`<origin> is no repository of the CI's ci.toml`): pushes start no pipeline and `run_jobs` is refused. Tell the user; do not work around it.

## 2. Tests before a push: the unit tier only

- Run the unit tests of the crates or packages your change touches, and nothing more: Rust `cargo nextest run -p <crate>` (nextest's default profile is the unit tier where `.config/nextest.toml` defines tiers), Python `uv run pytest tests/unit`.
- Never run the whole suite locally: no `cargo test --workspace`, `--profile ci` or `--profile integration`, no end-to-end run, even where the project's `AGENTS.md` lists them in a gate.
- Why: the push pipeline runs the tests again right after the push, on every platform, and a local whole-suite run costs minutes per change.
- What runs where: a push runs what its jobs select (zeughaus: the unit tier and the doctests); the nightly cron and a release tag also run the integration and end-to-end tiers. Their failures reach you like any other (section 5).
- An integration or end-to-end failure from nightly or release: reproduce it with `run_jobs` of that platform's check job, its test command temporarily switched to the integration tier and filtered to the failing test (section 4), not locally.

## 3. Debug a job or another platform

- Run `run_jobs` with the platform's pair: `jobs=["build-<p>", "check-<p>"]`. The platforms are in the job table (zeughaus: `linux-amd64`, `linux-arm64`, `darwin-arm64`, `windows-amd64`).
- `run_jobs` snapshots the checkout at `path` (default: the working directory) as it is on disk: HEAD's commit, uncommitted changes, untracked files that are not ignored, and the Cargo path dependencies beside it (`../<dir>`). It uploads that and runs the named jobs in the warm workspace of their place, the one the pipeline builds in. The second run is incremental.
- The call answers once the run has settled, within `wait_seconds` (default 540, at most 600). A run still going answers with how to continue: `ci_wait` with the `repo` and `number` it names.
- Read the answer. Every job has status, place and note. A failed job adds `cause:` (the first of `panicked at`, `error[`, `error:`, `FAILED` in its last 40 lines) and `excerpt:` (those lines).
- Need more: `ci_log` with `number` and `job`. `tail` counts lines from the end (default 200, at most 5000). `ci_log` has no filter: read the lines for `panicked at`, `error[`, `error:`, `FAILED`. `timestamps` starts every line with the seconds since the job's first output.
- A job that does not start says why in its note. The notes are listed in `skill://zeughaus-ci/reference/jobs.md`, "Job notes".
- Fix the code, then run the same jobs again.
- Runs for different platforms are independent: issue them as parallel `run_jobs` calls, one pair per platform. The jobs of a repository at one place share one workspace and run one after another, oldest first, so two runs for the same platform queue.
- Never cross-compile locally for it, build over ssh, push to test, or rerun a pipeline to look at one job.

## 4. Narrow a failure without a commit

- A manual run executes the jobs of the working tree. Edit the job file under `.zeughaus-ci/` temporarily (a test filter, another test tier such as nextest's `--profile ci`, `--nocapture`, `-v`, `RUST_BACKTRACE=1`) and `run_jobs` that one job.
- Undo the edit before committing: `git checkout -- .zeughaus-ci/<file>`, when the temporary edit was the file's only change.
- A job that hangs: `ci_log` with `timestamps` shows where its output stopped. `ci_timing` with `job` names the lines the longest pauses follow; while the job runs, the last pause lasts until now, so that line is the stuck step or test. Then `ci_cancel` the pipeline: it kills the running jobs and skips the rest.
- A job without `timeout_minutes` may run 240 minutes and holds its place's workspace meanwhile; every later job at that place waits.

## 5. After a push

- The zeughaus-ci extension watches the pipeline of the pushed commit. It reports the first failed job into the session as a message that starts your turn (job, place, cause, excerpt), and a success as a line with your next turn. Do not poll. `/zeughaus` shows the state in the TUI.
- On a failure: `ci_log` for the job's output; `run_jobs` with that job reproduces it from the working tree.
- Without the extension: `ci_wait`. Without `number` or `sha` it waits for the commit at HEAD, and for a pipeline that does not exist yet.
- A push starts a pipeline only when some job's `on` matches the ref (zeughaus: pushes to `main`, tags `v*`, the 03:00 cron). A push to another branch starts nothing; that is not a failure.
- Manual runs post no forge status and cannot be rerun. `ci_rerun` queues the event of a pipeline a push or a cron started, pinned to its commit.

## 6. What manual runs do not have

- Secrets: none of a job's `secrets` is given. A job that needs one to build cannot run there.
- A `needs` on a job you did not name is dropped: the job runs without waiting for it and finds no `$CI_INPUTS/<need>`. Name the build too when a check needs its output.
- Jobs on a Windows VM (`machine:<vm>` in the table) are refused: the whole run ends as `error` (`manual runs run on a runner's host or in a container; <job> runs on the machine <vm>`). A Windows runner's host is fine.
- A job's `on` is not consulted: any job of the working tree's `.zeughaus-ci/` can be named, one to 16 per call.
- The run lands in the channel a push to the default branch selects, and is never `clean`: workspaces stay warm.

## 7. Writing jobs

- Read `skill://zeughaus-ci/reference/pipelines.md` (how to shape a pipeline for this CI, zeughaus's own as the example, a review checklist) and `skill://zeughaus-ci/reference/jobs.md` (header keys, channels, placement, environment, job notes).
- Validate before pushing, where the binary exists: `zeughaus-ci check` (header rules, channels, ignored keys; placement too where the state directory has the CI's `ci.toml`) and `zeughaus-ci plan .zeughaus-ci push main` (the channel and the jobs an event selects).
- Without the binary, `run_jobs` validates on the CI: a header error ends the run as `error`, with the message as the pipeline's note.

## 8. Plans and proposals

- Plans and proposals of a repository live in the CI, numbered per repository. `docs` lists them (awaiting approval, the queue, in progress, open proposals, history); `doc_read` reads one with its text. The `<zeughaus-ci>` block names the open proposals and the plans that wait or run.
- A plan made from proposals names them in one line directly under its title: `Proposals: #3, #5`. Saving the plan, or executing it from its review, marks them planned.
- The user stores a finished plan with `/zeughaus save`; it waits in the desktop's Plans tab for approval, and the repository's master session executes approved plans one after another.
- A change another repository of the CI needs: `proposal_write` with `path` = that repository's checkout (or `repo` = its name), a one-line title and the body (what changed here and why, what the receiving project would change, what this project does afterwards, the checks). A repository the CI does not build gets a `PROPOSAL-<source>-<topic>.md` in its root instead; the tool's refusal says where.
- A session executing a plan from zeughaus (the master tells it the number) calls `plan_done` once its work is committed and pushed, and not before.
