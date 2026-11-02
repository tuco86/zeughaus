# Onboarding a project into the CI

How a project that the CI does not build yet joins it. The CI does the forge and secret work itself once the owner approves the request. Source in the zeughaus repository: `zeughaus-ci/src/onboard.rs` and `docs.rs`.

## When a project should join

- It has builds or tests that should run on every push, or on the nightly, on the owner's machines.
- It deploys to a host the CI's runners reach (sadala, a runner's host).
- It follows another repository of the CI (`push main of <repo>`), for example a site that rebuilds when its source project changes.

A project that only needs one change from another project does not need to join; the change goes to that project as a proposal.

## The rule

A project outside the CI gets an onboarding request, never a `PROPOSAL-*.md`. Ask the user first, then file it with `onboard_request`. As soon as the request is stored, the project has an inbox: `proposal_write` with `path` = its checkout stores proposals against it, and `docs`/`doc_read` read them. Plans for it wait until the request is done.

## Writing the request

`onboard_request` arguments:

| argument | value |
|---|---|
| `path` or `url` | The joining project's checkout (its `origin` names it) or its forge url, exactly one. The repository must be on the CI's Forgejo (`[onboarding] api` of ci.toml). |
| `name` | The repository's name in ci.toml, `[a-z0-9][a-z0-9-]*`, unique. |
| `title`, `body` | One line; the request in Markdown. |
| `default_branch` | Default `main`. |
| `grants` | Secret name to the event patterns it is released to, as ci.toml's grants: `{"DEPLOY_KEY": ["push main"]}`. A secret named here must already be in the CI's OpenBao document; the request fails with the name otherwise. |
| `prerequisites` | Host prerequisites the owner sets up by hand, one line each (a sudo rule, a served directory). |

The body covers:

- Purpose: what the project is and what its pipeline does.
- Events: `push main`, `tag v*`, `nightly`, and `push main of <repo>` when it follows another repository.
- Secrets per event, the same as `grants`, with why each is needed.
- Host prerequisites, the same as `prerequisites`, with who owns what.
- Whether the project brings its `.zeughaus-ci/` (it should; the starters below are a beginning).

## What approval does

The owner approves the request in the editor's Plans tab of the pending repository. The CI then, in order:

1. Checks that the admin token, the bot token and every granted secret are in its OpenBao document.
2. Adds its bot user as a collaborator with write access.
3. Generates a webhook secret (`<NAME>_WEBHOOK_SECRET`) and stores it in OpenBao, or reuses the stored one.
4. Creates the push webhook `<hook_url>/<name>`, or updates the one that is there.
5. Writes `ci/onboarded/<name>.toml` and reloads its configuration without a restart.
6. Queues the first `push` pipeline of the default branch.

Every step can run again. A step that fails returns the request to `open` with a note naming the step (`collaborator: ...`, `hook: ...`); `ci_repo` and `doc_read` show it. Fix the cause and approve again.

## Checking the result

In the joined project's checkout, `ci_repo` ends with `setup:` lines:

- `hook: last delivery <age> ago: push 202 queued`, or `no delivery recorded` until the next push.
- `bot: push access to <slug>`.
- `secrets: A ok, B missing`.
- `grants: every job secret is granted`, or each job secret that ci.toml grants to no event.

## Starter `.zeughaus-ci/` headers

Only keys of `reference/jobs.md`. Platforms and runners are those `ci_status` lists.

A Rust workspace, a build and check pair per platform:

```sh
# build-linux-amd64.sh
# /// ci
# on = ["push main", "tag v*", "nightly"]
# platform = "linux-amd64"
# image = "trixie"
# ///
cargo build --workspace --locked
```

```sh
# check-linux-amd64.sh
# /// ci
# needs = ["build-linux-amd64"]
# platform = "linux-amd64"
# image = "trixie"
# ///
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

A static site built in a container and deployed on sadala:

```sh
# build.sh
# /// ci
# on = ["push main"]
# image = "site"
# ///
zola build --output-dir "$CI_OUTPUT/public"
```

```sh
# deploy.sh
# /// ci
# needs = ["build"]
# machine = "sadala"
# ///
rsync -a --delete "$CI_INPUTS/build/public/" /var/www/example.doodleshnookie.net/
```

A service with a deploy job and an install script behind a sudo rule:

```sh
# deploy.sh
# /// ci
# needs = ["build"]
# machine = "sadala"
# secrets = ["DEPLOY_KEY"]
# ///
sudo -n /usr/local/sbin/install-example "$CI_INPUTS/build/example"
```

A follower that rebuilds when another repository's `main` is pushed:

```sh
# rebuild.sh
# /// ci
# on = ["push main of weida"]
# image = "site"
# ///
echo "following $CI_UPSTREAM_REPO at $CI_UPSTREAM_SHA"
```

## Host prerequisites that exist

- A sudo rule plus an install script: the deploy job calls `sudo -n <script>`, the rule allows exactly that script for the runner user (assistant-rs, eisen-und-eid-server).
- A served directory owned by the runner user `zeughaus`, which the deploy job writes into (weida-site, `/var/www/weida.doodleshnookie.net`).
