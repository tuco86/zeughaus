---
name: zeughaus-ci
description: "Repo has .zeughaus-ci/: debug a job or platform with run_jobs, CI state, after a push, write or review CI jobs."
---

# zeughaus-ci

zeughaus-ci is the user's own CI. Its runners are the machines they own, one per platform. A repository's jobs are scripts in its `.zeughaus-ci/` folder. You reach the CI through the tools of the MCP server `zeughaus`: `ci_repo`, `ci_status`, `ci_pipelines`, `ci_pipeline`, `ci_log`, `ci_timing`, `ci_wait`, `ci_rerun`, `ci_run`, `ci_cancel`, `run_jobs`, `docs`, `doc_read`, `proposal_write`, `onboard_request`, `plan_done`, and for the data a job keeps between pipelines (`state = [..]` in its header) `ci_state`, `ci_state_get`, `ci_state_delete`.

## 1. Orient

- Read the `<zeughaus-ci>` block of the system prompt first. It is the state at session start: which repository of the CI this checkout is, every job with the runner and place it runs at, whether HEAD and the default branch were green. It does not change during the session.
- Call `ci_repo` for the state now: the pipeline of HEAD, the channels, the jobs, the runners, the platforms. `ci_status` shows the whole CI: the CI's build, every channel, every runner online or offline with the build it runs, the queue. `ci_pipelines` without `channel` lists every channel's pipelines, each line starting with its channel.
- In the checkout, leave `repo` out of the other `ci_*` tools. It defaults to the repository of the working directory's checkout.
- Job names are the file stems of `.zeughaus-ci/`: `build-linux-amd64.sh` is the job `build-linux-amd64`. `run_jobs`, `ci_log` and `needs` use these names.
- Read a job row as: name, runner, where on it (`host`, `image:<x>` or `machine:<vm>`), then `on <patterns>` or `needs <jobs>`, then `channels <names>`, `secrets <names> (none in run_jobs)` and `run_jobs cannot run it` when they apply. A job the CI cannot place says `cannot be placed: <why>` in place of the runner.
- The job rows are read at the commit the `jobs at <sha7>` line names: HEAD once the CI has fetched it, else the default branch's head. Your uncommitted edits to `.zeughaus-ci/` are not in them; `run_jobs` reads those.
- `HEAD <sha7>: no pipeline` means the commit is not pushed, or no job's `on` selects it.
- An origin the CI does not know (`<origin> is no repository of the CI's ci.toml`): pushes start no pipeline and `run_jobs` is refused. Ask the user, then file an onboarding request (`skill://zeughaus-ci/reference/onboarding.md`); do not work around it. An origin with a pending request says so (`onboarding request #<n> for <name> is open|approved`, and the last failure).
- `ci_repo` ends with `setup:` lines: the hook's last delivery, the bot's push access, the secrets the repository names and whether every job secret is granted to some event.

## 2. Tests before a push: the unit tier only

- Run the unit tests of the crates or packages your change touches, and nothing more: Rust `cargo nextest run -p <crate>` (nextest's default profile is the unit tier where `.config/nextest.toml` defines tiers), Python `uv run pytest tests/unit`.
- Never run the whole suite locally: no `cargo test --workspace`, `--profile ci` or `--profile integration`, no end-to-end run, even where the project's `AGENTS.md` lists them in a gate.
- Why: the push pipeline runs the tests again right after the push, on every platform, and a local whole-suite run costs minutes per change.
- What runs where: a push runs what its jobs select (zeughaus: the unit tier and the doctests); the nightly and a release tag also run the integration and end-to-end tiers. Their failures reach you like any other (section 5).
- An integration or end-to-end failure from nightly or release: reproduce it with `run_jobs` of that platform's check job, its test command temporarily switched to the integration tier and filtered to the failing test (section 4), not locally.

## 3. Debug a job or another platform

- Run `run_jobs` with the platform's pair: `jobs=["build-<p>", "check-<p>"]`. The platforms are in the job table (zeughaus: `linux-amd64`, `linux-arm64`, `darwin-arm64`, `windows-amd64`).
- `run_jobs` snapshots the checkout at `path` (default: the working directory) as it is on disk: HEAD's commit, uncommitted changes, untracked files that are not ignored, and the Cargo path dependencies beside it (`../<dir>`). It uploads that and runs the named jobs in the warm workspace of their place, the one the pipeline builds in. The second run is incremental.
- The call answers once the run has settled, within `wait_seconds` (default 540; a larger value waits 600). A run still going answers with how to continue: `ci_wait` with the `repo` and `number` it names. It also answers early when a running job reaches 1x, 2x, ... of its expected time (median of its last 5 successful runs there, at least 1 minute; `expected_minutes` overrides it), with the job's last lines and the other jobs on its machine: decide to wait on (call again) or `ci_cancel`. Nothing stops a job before its `timeout_minutes`.
- A wait longer than a few minutes: run `zeughaus-runner ci wait <repo> <number>` (optionally a job name after the number) as a background bash command (`async`), with a generous timeout (it answers at each multiple of the job's expected time by itself), and keep working. It prints the pipeline once it has settled and exits 0, whatever the pipeline's status; 4 when a running job reached a multiple of its expected time (stdout says which; run it again to wait on); 2 when it could not ask or ran out.
- Read the answer. Every job has status, place and note. A failed job adds `cause:` and `excerpt:`. The cause is the first of `panicked at`, `error[`, nextest's `FAIL [` (also `TIMEOUT [`, `SIGSEGV [`, `SIGABRT [`), `error:`, `FAILED`; the excerpt is the last 40 lines, led by the first panic and its message (then `...`) when the panic is further up, so the cause is the panic of a failed test run.
- A finished job ends its line with its load, `(3.4x expected, shared its runner with 2 other jobs)`, and a `shared with:` line names those jobs (`<repo>#<n>/<job>`); a finished pipeline leads with `load:` of its failed or slowest job, and `ci_status` adds it to the channel's line. A slow job that shared its runner was likely starved, not broken.
- Need more: `ci_log` with `number` and `job`. To find something in the output, use `pattern` (a Rust regex, over the whole stored output or within `lines`) with `context` (lines around each match, at most 20); for a test run `panicked at|FAIL \[|error(\[|:)`. Never fetch `tail: 5000` to search it. `tail` counts lines from the end (default 200, at most 5000), `lines` names lines like read does, `timestamps` starts every line with the seconds since the job's first output.
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

- The zeughaus-ci extension watches the pipeline of the pushed commit. It reports the first failed job into the session as a message that starts your turn (job, place, cause, excerpt), each multiple of its expected time a running job reaches (job, place, elapsed, last lines, other jobs on its machine; you decide to wait on or `ci_cancel`), and a success as a line with your next turn. Do not poll. `/zeughaus` shows the state in the TUI.
- On a failure: `ci_log` for the job's output; `run_jobs` with that job reproduces it from the working tree.
- Responsibility: the session that starts or waits on a pipeline holds it (the extension and the MCP tools claim it for your zeughaus terminal). A failed pipeline's `responsible:` line in `ci_pipeline` and `ci_wait` (and the extension's failure report) names who fixes it: the holder of the earliest earlier pipeline of the channel that failed one of the same jobs, else the pipeline's own. When that is another session, leave the fix to it and `ci_wait` for the channel's next pipeline instead of fixing it in parallel. `zeughaus-runner ci responsible <repo> <n>` asks the same from a script.
- Without the extension: `ci_wait`, or `zeughaus-runner ci wait <repo> <number|sha>` as a background command for a long one. Without `number` or `sha`, `ci_wait` waits for the commit at HEAD, and for a pipeline that does not exist yet.
- A push starts a pipeline only when some job's `on` matches the ref (zeughaus: pushes to `main`, tags `v*`, the nightly). A push to another branch starts nothing; that is not a failure.
- Manual runs post no forge status and cannot be rerun. `ci_rerun` queues the event of a pipeline a push or a nightly started, pinned to its commit, and answers with the new pipeline's number.
- `ci_run` queues a pipeline of the default branch at its current head: `event: "push"` as a push would, `event: "nightly"` to run the repository's nightly now. The CI runs one nightly at a time across all repositories (its round starts at ci.toml's `[nightly] at`), so a nightly may wait behind another. It answers with the new pipeline's number. Never ssh to the CI host to queue an event.
- Whether the hosts installed a published build: `ci_status` shows each runner's build; do not run `update status` over ssh.

## 6. What manual runs do not have

- Secrets: none of a job's `secrets` is given. A job that needs one to build cannot run there.
- A `needs` on a job you did not name is dropped: the job runs without waiting for it and finds no `$CI_INPUTS/<need>`. Name the build too when a check needs its output.
- Jobs on a Windows VM (`machine:<vm>` in the table) are refused: the whole run ends as `error` (`manual runs run on a runner's host or in a container; <job> runs on the machine <vm>`). A Windows runner's host is fine.
- A job's `on` is not consulted: any job of the working tree's `.zeughaus-ci/` can be named, one to 16 per call.
- The run lands in the channel a push to the default branch selects, and is never `clean`: workspaces stay warm.

## 7. Writing jobs

- Read `skill://zeughaus-ci/reference/pipelines.md` (how to shape a pipeline for this CI, zeughaus's own as the example, a review checklist), `skill://zeughaus-ci/reference/jobs.md` (header keys, channels, placement, environment, job notes) and, for a project that is to join the CI, `skill://zeughaus-ci/reference/onboarding.md` (the request, what approval does, starter headers).
- Validate before pushing, where the binary exists: `zeughaus-ci check` (header rules, channels, ignored keys; placement too where the state directory has the CI's `ci.toml`) and `zeughaus-ci plan .zeughaus-ci push main` (the channel and the jobs an event selects).
- Without the binary, `run_jobs` validates on the CI: a header error ends the run as `error`, with the message as the pipeline's note.

## 8. Tickets

- Work on a repository is a ticket in the CI, numbered per repository, which keeps its number from proposal through plan to execution: `open` (a proposal), `ready` (released for planning; the CI opens a plan session once the tickets it depends on are done), `planning`, `review` (its plan waits for the user), `approved` (queued for a master), `in_progress`, `done`; or `dismissed`, or `merged` into the ticket whose plan includes it. `docs` lists them (awaiting review, the queue, in progress, planning, open, history; `(after #n)` names what a ticket waits for); `doc_read` reads one with its proposal, plan and links. The `<zeughaus-ci>` block names the open tickets and those that are planned, wait or run.
- A ticket is one Markdown text: its first line `# <title>`, then `Depends on:` (alias `Blocked by:`) and `Related:` lines with `#12` (this repository) or `#weida-12` (another); title and links are read from it. Every ticket can be pre-approved in zeughaus: its plan is approved as it arrives.
- A plan that also does the work of other open tickets names them in one line directly under its title: `Includes: #3, #5`. They are merged into its ticket and close with it.
- Work another repository of the CI has to do, or a later ticket of this one, is a section `## <repo> - <title>`, optionally ending in a label `#<label>`: it becomes a ready ticket in that repository when the plan is approved, and is left out of the plan the master executes. Its `Depends on:` / `Related:` lines (among its first lines) name `#self` (this plan's ticket), `#<label>` (another such section), `#12` or `#<repo>-12`. The section is the whole ticket text there: what is now available, what to change, its checks. submit_plan refuses a section whose references do not resolve.
- A plan session ends with `submit_plan`: the plan is stored on its ticket and the session ends. The user reviews it in zeughaus, deleting or annotating sections, and approves it or sends the review back with Refine, which resumes the same session with the review attached. `/zeughaus save` stores the session's plan as a new ticket in review. The CI dispatches each approved ticket to a free runner matching the plan's `Runs on` line (one label selector per machine, the first hosting the session, for example `Runs on: os=macos`; without it any runner that hosts sessions), one master per runner, so several plans may run at once.
- A change another repository of the CI needs: `proposal_write` with `path` = that repository's checkout (or `repo` = its name) and the body, a ticket in Markdown that starts with its `# <title>` line (there is no separate title): what changed here and why, what the receiving project would change, what this project does afterwards, the checks. It is an open ticket there. A repository the CI does not build yet gets an onboarding request first (`onboard_request`, whose body also starts with its `# <title>` line); `proposal_write` stores tickets against it once the request exists.
- A session executing a ticket's plan from zeughaus finds its number in `ZEUGHAUS_PLAN` (`<repo>#<n>`) and calls `plan_done` once its work is committed and pushed, and not before. A session the CI opened has a `<zeughaus-session>` block naming its runner, its machines (`ZEUGHAUS_MACHINES`, `name=ssh destination`) and the other runners with their labels.

## 9. The Claude sign-in

- omp's Anthropic token comes from the CI, not from a `/login anthropic` per machine: the CI holds the Claude subscription's refresh token, renews the access token, and `zeughaus-runner claude token` asks the CI for the current one (`models.yml`'s `providers.anthropic.apiKey`). omp runs it again after a 401, which is how it follows a renewal.
- `/zeughaus claude` shows where the sign-in stands; `/zeughaus claude login` (or `zeughaus-runner claude login`, or the Overview's row in the editor) signs in again through this machine's browser. A sign-in lasts about 30 days. Never run omp's own `/login anthropic`.

## 10. Secrets in sessions and jobs

- A zeughaus work shell and agent session already have their project's zeughaus secrets in the environment (`secret/zeughaus/<origin>/{omp,shell}`, then `secret/zeughaus/{omp,shell}`); a pane shell has `secret/zeughaus/shell`. What the terminal's own environment sets wins.
- The owner's omp secrets are not in the environment: the `secrets` tool lists documents and field names below `secret/hannes/omp` (or another path), and `${secret:hannes/omp/<doc...>/<FIELD>}` in a bash command is replaced by the value when the command runs, in a subshell (not inside single quotes). Other paths are refused.
- A job's `secrets` come from `secret/zeughaus/<origin>/shell`, then `secret/zeughaus/shell`, through a single-use job token the CI hands the runner; its values show as `*` in the job's log. Store a new one there (`bao kv patch -mount=secret zeughaus/<origin>/shell NAME=-`) and grant it in ci.toml.
