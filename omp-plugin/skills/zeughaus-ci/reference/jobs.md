# Job files, channels, placement and environment

What zeughaus-ci reads from a repository's `.zeughaus-ci/` folder and what it gives a job. These are the rules `zeughaus-ci check` enforces (source in the zeughaus repository: `zeughaus-ci/src/header.rs`, `pipeline.rs`, `channels.rs`). A violation fails the pipeline's setup: status `error`, the message in the pipeline's note.

## A job file

| item | rule |
|---|---|
| file | A `.sh` or `.ps1` file of `.zeughaus-ci/` that has a header block. A file without the block is a helper: sourced by jobs, never run on its own. A block in a file of another extension is an error |
| name | The file stem, `[a-z0-9][a-z0-9-]*`, unique (`a.sh` and `a.ps1` clash). `needs`, `run_jobs` and `ci_log` use it |
| header | TOML in comment lines, from a line `# /// ci` to a line `# ///`. Every line between starts with `#`; one space after it is dropped. An unknown key is an error |
| interpreter | `.ps1`: PowerShell. `.sh`: `bash` for the shebang `#!/bin/bash`, `#!/usr/bin/bash` or `#!/usr/bin/env bash`; `sh` without a shebang or for `#!/bin/sh`, `#!/usr/bin/sh`, `#!/usr/bin/env sh`. Any other shebang is an error |
| start | After the checkout, in the workspace root (`$CI_WORKSPACE`), as `<interpreter> .zeughaus-ci/<file>`. Exit code 0 is success |

A header with an `env` table:

```sh
# /// ci
# needs = ["build"]
# timeout_minutes = 30
# [env]
# GITHUB_REPO = "owner/name"
# ///
```

## Header keys

| key | value | rule |
|---|---|---|
| `on` | list of patterns | `push <glob>`, `tag <glob>` or `cron <expr>`. A glob is one word; `*` matches any run of characters, `/` and nothing included, everything else is literal (`main`, `v*`, `release/*`). A cron expression has exactly five fields; it runs against the default branch's head, in the CI host's local time. `cron *` is valid only in channels and grants. Empty when the job has `needs` |
| `needs` | list of job names | Each names another job of the folder; no cycles. The job is selected only when all its needs are selected, and is skipped when one of them fails or is skipped. A need that succeeded and left output is staged as `$CI_INPUTS/<need>` |
| `channels` | list of channel names | Only with `needs` (a job with `on` is selected by its own patterns). The job joins a pipeline only in these channels; without the key, in every channel that selects what it needs. A name is a channel of `channels.toml`, or `tags` or `cron` (`zeughaus-ci check` refuses others) |
| `image` | name | `[a-z0-9][a-z0-9-]*` with a file `.zeughaus-ci/<image>.Containerfile`. The job runs in a container built from it (build context `.zeughaus-ci/`) on the platform's runner, else on the default runner. A unix runner only. Exclusive with `machine` |
| `machine` | name | A runner of ci.toml (the job runs on its host) or a machine (Windows VM) a runner lists. Exclusive with `image` and `platform` |
| `platform` | name | `[a-z0-9][a-z0-9-]*`, a key of ci.toml `[platforms]`, whose value is the runner or VM that builds it. Exclusive with `machine`. The job sees it as `CI_PLATFORM` |
| `secrets` | list of names | Each `[A-Z_][A-Z0-9_]*`. A secret reaches the job as an environment variable of that name when the repository's `grants` in ci.toml release it to the event (`push main`, `tag v*`, `cron *`). Otherwise the job fails with `secret <name> is not granted for <event> <ref>`. A manual run gives none |
| `when_busy` | `wait`, `freeze`, `run` | Default `wait`. Matters only on a runner with a `[busy]` table (GPU load, a desktop someone uses). `wait` and `freeze` are assigned only while the runner is free. A running `freeze` job is paused while the runner is busy (`podman pause`, or a VM stop) and resumed when it is free again; a running `wait` job is left alone. `run` is assigned even while the runner is busy and never paused. `freeze` needs `image`, `machine` or `platform` in the header and a container or a VM at placement: on a runner's host it is refused when the pipeline is set up, because a host job cannot be paused. Apple's `container` cannot pause either: the job runs on |
| `env` | table of strings | More variables for the job. Names `[A-Z_][A-Z0-9_]*`, none starting with `CI_` or `ZEUGHAUS_` |
| `timeout_minutes` | integer | 1..=1440, default 240. At the deadline the job gets SIGTERM and, 60 s later, SIGKILL. Its note is `timed out after <n> min` |
| `context`, `cache` | any | Parsed and ignored: a repository's jobs at one place share one workspace and a checkout keeps ignored files. `zeughaus-ci check` warns; remove them |

Rules across keys:

- Exactly one of `on` and `needs` is non-empty. A job with `on` cannot wait for another job: order is `needs` and nothing else.
- A `.ps1` job needs `machine` or `platform`. A `.ps1` runs on a Windows runner's host or in a VM; a `.sh` on a unix runner's host or in a container. A mismatch fails the setup.
- A name in `needs` must be a job of the folder and not the job itself.

## Which jobs an event runs

- First the jobs whose `on` matches the event; then, repeatedly, every job with `needs` whose needs are all selected and whose `channels` is empty or names the event's channel.
- Nothing selected: no pipeline is made (the CI only logs `no jobs`). A push to a ref no job's `on` names starts nothing. A push to the default branch whose commit has no `.zeughaus-ci/` is recorded as `no-jobs` and posts an error on the forge.
- A push or tag event runs the commit it names; a cron event runs the default branch's head; a manual run runs its uploaded working tree.

## Placement

| header | the job runs | workspace under `ci/work/<repo>/` |
|---|---|---|
| none of `image`, `machine`, `platform` | on the default runner's host | `host` |
| `image = "i"` | in container `i` on the default runner | `image-i` |
| `platform = "p"` | where `[platforms]` puts `p`: a runner's host, or a VM of a runner | `host` or `machine-<vm>` |
| `platform = "p"` and `image = "i"` | in container `i` on the platform's runner; a platform that is a VM is an error | `image-i` |
| `machine = "<runner>"` | on that runner's host | `host` |
| `machine = "<vm>"` | on the VM, on the runner that lists it (`.ps1`) | `machine-<vm>` |

- `ci_repo` and the `<zeughaus-ci>` block print where as `<runner> <at>`, with `at` one of `host`, `image:<x>` or `machine:<vm>`. `zeughaus-ci check` prints the header's own label: `host`, `image:<x>`, `machine:<x>`, `platform:<p>` or `platform:<p> image:<x>`.
- A Windows runner runs `.ps1` jobs on its host and nothing else: no containers, no VMs. A unix runner runs `.sh` jobs on its host or in a container, and `.ps1` jobs on the VMs it lists.
- A changed Containerfile is a new image tag, built before the next job that uses it: expect one slow run. The workspace keeps its tree, because it is named after the image without the tag.
- A job whose runner is not connected waits; the notes below say how long.

## Workspaces and the checkout

- One workspace per repository, runner and place: `<runner state dir>/ci/work/<repo>/<ws>`, `/work` inside a container. `<ws>` is `host`, `image-<image>` or `machine-<vm>`.
- Every job of the repository at that place uses it, pipeline jobs and manual runs alike, one at a time, oldest first (by pipeline number, then position in the pipeline). A job finds what the one before left, `target/` included.
- Before each job: fetch, `git checkout --force --detach`, `git clean -ffd`. Tracked files are reset, untracked files removed, ignored files stay. In a pipeline of a `clean` channel the first job at each place gets `git clean -ffdx`: nothing stays.
- A manual run checks out its uploaded working tree the same way and never wipes. Its sibling checkouts sit in `<workspace>/.zci-siblings/<dir>`, which `git clean` leaves alone.
- A runner deletes the least recently used workspaces beyond its `budget_gb` (50). A workspace is a cache, not state.

## channels.toml

```toml
[[channel]]
name = "dev"
on = ["push main"]

[[channel]]
name = "nightly"
on = ["cron *"]
clean = true
```

| key | rule |
|---|---|
| `name` | `[a-z0-9][a-z0-9-]*`, unique |
| `on` | Non-empty list of patterns as in a job's `on`; `cron *` (any cron event) is allowed here |
| `clean` | Boolean, default false. The first job of a pipeline of this channel at each place starts its workspace from nothing |

- The first channel in file order whose `on` matches the event names the pipeline's channel. Unknown keys are an error.
- No match, or no file: a push goes to a channel named after its branch, a tag to `tags`, a cron event to `cron`. A manual run goes to the channel a push to the default branch selects.
- A channel sorts pipelines (`ci_pipelines` takes one; the CI keeps the 200 newest per channel) and sets `clean`. It starts nothing: a job's `on` does.

## Job environment

Set by the CI:

| variable | value |
|---|---|
| `CI` | `true` |
| `ZEUGHAUS_CI` | `1` |
| `CI_REPO` | The repository's name in ci.toml |
| `CI_REPO_SLUG` | Its `slug`, `owner/name`; empty when ci.toml gives none |
| `CI_EVENT` | `push`, `tag`, `cron` or `manual` |
| `CI_REF` | Branch or tag name without `refs/`; the default branch for `cron` and `manual` |
| `CI_SHA` | The commit id (a tag's commit). For `manual`, the snapshot commit of the uploaded working tree, not HEAD |
| `CI_PIPELINE` | The pipeline number |
| `CI_JOB` | The job's name |
| `CI_ACTOR` | Who caused it: the forge user of a push or tag, `cron`, `rerun of #<n>`, `manual` for `zeughaus-ci run`, `<user>@<host>` of a manual run |
| `CI_PLATFORM` | The job's `platform`; set only when the header has one |
| `CI_CRON` | The cron expression, whitespace normalized; set only for `cron` events |

Set by the runner:

| variable | value |
|---|---|
| `CI_WORKSPACE` | The place's workspace, where the job starts. Container: `/work` |
| `CI_OUTPUT` | Directory for the job's artifacts. It exists, empty, when the job starts, and lies outside the workspace. A successful run that left files keeps them while a job that needs them can still run; nothing else is kept |
| `CI_INPUTS` | One directory `<need>/` for each job of `needs` that succeeded and left output. Container: `/ci/inputs` |
| `CI_CPUS`, `CARGO_BUILD_JOBS` | The CPU count of the place: the runner's, the container's or the VM's |
| `CI_SIBLINGS` | Manual runs only: the directory of the sibling checkouts, `<workspace>/.zci-siblings` (`/work/.zci-siblings` in a container). `$CI_SIBLINGS/<dir>` is the path dependency `<dir>` |

Besides these: the header's `env`, and the granted `secrets` by name. A `.ps1` job reads all of them as `$env:NAME`.

## Statuses

| job status | meaning |
|---|---|
| `pending`, `waiting` | Not assigned yet; `waiting` is held back, the note says why |
| `starting` | Handed to a runner that has not reported the run started |
| `running`, `frozen` | Running; paused because the runner is busy |
| `succeeded`, `failed`, `skipped` | Ended |

| pipeline status | meaning |
|---|---|
| `running` | Some job has not ended |
| `succeeded` | Every job succeeded |
| `failed` | Some job did not succeed (it failed or was skipped) and the pipeline was not cancelled |
| `error` | The setup failed: fetch, `.zeughaus-ci/` validation, placement; for a manual run also an unknown job name, a missing `.zeughaus-ci/` or a job on a VM |
| `no-jobs` | A push to the default branch whose commit has no `.zeughaus-ci/` |
| `cancelled` | Stopped by `ci_cancel` |

## Job notes

| note | meaning |
|---|---|
| `waiting for runner <R>` | Its runner is not connected. After the runner's `wait_minutes` (15 unless ci.toml says otherwise) the job is `skipped` with `runner <R> offline` |
| `waiting for runner <R> (artifacts of <need>)` | The runner that holds a need's output is not connected; skipped the same way |
| `runner <R> busy` | The runner is busy (GPU load, or set busy by hand) and the job's `when_busy` is not `run` |
| `workspace <ws> in use by #<n>` | Another job at the same place is running, in pipeline `#<n>` |
| `workspace <ws> queued for #<n>`, `waiting for <job> in workspace <ws>` | An earlier job at the same place goes first |
| `skipped: <need> failed (#<n>)`, `skipped: <need> skipped (#<n>)` | A need did not succeed |
| `cancelled` | The pipeline was cancelled |
| `failed with exit <n>` | The script's exit code |
| `failed: checkout (exit 70)` | The launcher could not enter or check out the workspace; read the head of the log |
| `failed: container image build (exit 71)` | The image build failed, or the container system did not start |
| `failed: copying artifacts from the guest (exit 74)`, `failed: guest unreachable (exit 75)` | VM jobs |
| `timed out after <n> min` | `timeout_minutes` was reached |
| `killed`, `died from a signal` | Killed by a cancel, or died from a signal |
| `runner <R> lost the run`, `runner <R> lost` | The runner restarted and does not know the run, or stayed away past the job's timeout plus 15 minutes |
| `runner <R> offers no containers`, `runner <R> offers no machine <vm>`, `runner <R> is not configured` | The placement does not match what the runner offers |

A job that holds secrets keeps its `cause` out of the forge's commit status; `ci_pipeline` still shows it.

## What ci.toml supplies

It lives on the CI host, not in the repository; `ci_repo` and `ci_status` show the result.

| key | meaning |
|---|---|
| `default_runner` | Runs the jobs that name no `machine` or `platform`, and the containers of jobs without a target |
| `[runners.<name>]` | `fingerprint`, `os` (`unix`, the default, or `windows`), `machines` (the VMs it boots), `wait_minutes` (15) |
| `[platforms]` | Platform name to a runner or a machine of one |
| `[repos.<name>]` | `url` (the repository's identity: an upload's `origin` must be the same repository), `forge` (`github`, `forgejo` or `none`), `slug`, `api`, `default_branch` (`main`), tokens, `grants` (secret name to event patterns) |
