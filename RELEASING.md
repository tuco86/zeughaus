# Releasing

Checklist for cutting a release of the zeughaus workspace. Every crate of the
workspace is versioned together from `[workspace.package]` and published in
one go; the wezterm crates under `third_party/` have their own versions and are
published only when they change.

`main` carries the next version with a `-dev` suffix (`0.1.0-dev`); a release
replaces it with the release number (`0.1.0-alpha.1`, `0.1.0`, ...) and a new
`-dev` cycle begins afterwards. The `-dev` version is never published. Under
semver's pre-release ordering `0.1.0-alpha.N < 0.1.0-beta.N < 0.1.0-dev <
0.1.0-rc.N < 0.1.0`, so the next alpha after a `-dev` cycle sorts below it;
that is harmless because nothing resolves against `-dev` but the workspace
itself.

## 1. Pick the version

Pre-1.0 under Cargo's rules: a public-API break is a minor bump
(`0.1.x` -> `0.2.0`), anything else a patch bump. Before `0.1.0` the releases
are pre-releases: `0.1.0-alpha.N`, then `0.1.0-beta.N`, `0.1.0-rc.N`.

## 2. Bump the version

The root `Cargo.toml` holds it in `[workspace.package] version` and in the
`version` of every in-tree entry of `[workspace.dependencies]`
(`zeughaus-*`, `iced_tabs`, `iced_terminal`): crates.io strips the `path` of a
dependency on publish and keeps only its `version`. The crate manifests
inherit with `version.workspace = true` and need no edit. Replace every
occurrence of the old version string in the root manifest, then sync the lock
file:

```bash
cargo update --workspace
```

`weida` is a path dependency on the sibling checkout with the version of a
published weida release; a zeughaus release needs that weida version on
crates.io first.

## 3. Update the CHANGELOG

- Rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD` and add a fresh empty
  `## [Unreleased]` above it.
- Add the link reference at the bottom:
  `[X.Y.Z]: https://github.com/tuco86/zeughaus/releases/tag/vX.Y.Z`

## 4. Run every gate

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check --target wasm32-unknown-unknown -p zeughaus
cargo publish --workspace --dry-run
```

The dry run packages every crate and builds it from the packaged sources
against the other packaged crates, which catches a file a crate reaches outside
its own directory and a dependency that only compiles through the workspace's
feature unification. It resolves crates that are not part of this workspace
from crates.io, so a changed `third_party` crate has to be published first
(step 6).

## 5. Commit and tag

```bash
git commit -am "chore(release): X.Y.Z"
git tag -a vX.Y.Z -m "Release X.Y.Z"
git push origin main vX.Y.Z
git push github main vX.Y.Z
```

## 6. Publish

A `third_party` crate goes out only when it changed, with its own version
bumped, before the zeughaus workspace:

```bash
(cd third_party && cargo publish -p zeughaus-<crate>)
```

Then the workspace, in dependency order:

```bash
cargo publish --workspace
```

crates.io limits how fast one account creates *new* crates (a burst of five,
then one every ten minutes); new versions of existing crates are far less
limited, so this bites on a first release and when a crate is added. A publish
that hits the limit stops with the time to retry after; continue with
`cargo publish -p <crate>` for the remaining crates, in the order the first
run printed, once that time has passed.

Publish from the release tree (version `X.Y.Z`), before step 7.

## 7. Begin the next dev cycle

Replace the release version in the root `Cargo.toml` with the next
`X.Y.Z-dev`, then:

```bash
cargo update --workspace
git commit -am "chore: begin X.Y.Z-dev development cycle"
git push origin main
git push github main
```
