# Releasing Rumble

Rumble uses a **two-level versioning model**:

1. **Product release** — a single umbrella git tag `vX.Y.Z`. This is what end
   users download. Pushing the tag triggers `.github/workflows/release.yml`,
   which builds and publishes the Linux + Windows binaries (`server`,
   `mumble-bridge`, `rumble-damascene`), their `.sha256` sums, and the GHCR
   docker images (`rumble-server`, `mumble-bridge`). **All release artifacts are
   named from the tag** (`${TAG#v}`) — not from any crate's `Cargo.toml`
   version.

2. **Per-crate versions** — each crate's `version` in its own `Cargo.toml`,
   bumped *independently* as that crate changes. These are **informational
   bookkeeping**: nothing is published to crates.io and every internal
   dependency is a `path = "../foo"` dep with no `version =` requirement, so
   Cargo never reads these numbers for resolution. They exist to communicate, at
   a glance, *what moved and how much* between releases.

The product tag is **not** required to equal any single crate's version. It is
the product version; the crates version themselves underneath it. (For example,
`v0.3.1` was a packaging-only release while every crate stayed at `0.3.0`.)

## Per-crate semver policy

Bump each crate independently using normal semver judgement, scoped to *that
crate's* public surface / behavior:

- **patch** (`x.y.Z`) — bug fixes, internal refactors, no behavior change.
- **minor** (`x.Y.0`) — new features or user-visible behavior changes that are
  backward compatible.
- **major** (`X.0.0`) — breaking changes to the crate's API or behavior.

Special cases:

- **`rumble-protocol`** is the wire contract. Treat a version bump here as a
  signal about wire compatibility — bump minor for backward-compatible additions
  (new optional proto fields/messages), major for anything that breaks an older
  client/server pairing. **Leave it untouched when the protocol doesn't change**,
  even across a busy release; a stationary `rumble-protocol` version is a
  promise that old and new peers still interoperate.
- Crates with no changes since the last tag keep their version. Don't bump in
  lockstep "for tidiness" — the whole point is that an unchanged version means
  unchanged code.

## Cutting a release

1. **See what changed** since the last release tag:

   ```bash
   cargo xtask release-status
   ```

   It lists every crate as `unchanged` / `bumped` / `NEEDS BUMP` / `new`, with
   the file-change count and the old→new version for bumped crates. It exits
   non-zero if any crate changed since the tag but still carries its tag-time
   version — i.e. you forgot to bump it.

2. **Bump the crates it flags.** Edit each crate's `version` in its `Cargo.toml`
   per the semver policy above, then refresh the lockfile:

   ```bash
   cargo update --workspace
   ```

3. **Re-run `cargo xtask release-status`** until it reports
   "All changed crates have been version-bumped" (exit 0).

4. **Pick the product version.** The umbrella `vX.Y.Z` generally tracks the
   largest crate bump in the release (e.g. any crate going minor → product minor;
   only patches → product patch). Use a pre-release suffix (`v0.5.0-rc.1`) to
   ship a marked GitHub pre-release + a non-`:latest` docker tag.

5. **Commit, tag, push:**

   ```bash
   git commit -am "release: vX.Y.Z"
   git tag vX.Y.Z
   git push origin master --tags
   ```

   The `release` workflow takes it from there. Pre-release tags (any `-` suffix,
   e.g. `-rc.1`) are published as GitHub pre-releases and do **not** move the
   docker `:latest` tag.
