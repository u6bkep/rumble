# Napkin

## Corrections
| Date | Source | What Went Wrong | What To Do Instead |
|------|--------|----------------|-------------------|

## User Preferences
- Team sprints: use worktrees for parallel development
- Code review required before merging
- GUI changes need harness CLI verification + critic approval
- Format with `cargo +nightly fmt`
- imports_granularity = "Crate" in rustfmt

## Patterns That Work
- Worktree-based parallel dev: `git worktree add ../name -b branch` works well for team sprints
- Code review catches incomplete coverage (rwlock fix missed audio_task.rs)
- `replace_all: true` in Edit tool efficiently updates all instances (28 unwraps in one call)
- Harness CLI for screenshot verification: `up --screenshot`, `client screenshot --crop`

## Patterns That Don't Work
- Trusting sub-agent scope: reviewer correctly found rwlock fix was incomplete (audio_task.rs had 28 more instances)
- Private helpers in one file can't be shared: needed `pub(crate)` for cross-file reuse

## Domain Notes
- Rumble = voice chat app (Mumble-like) in Rust
- QUIC transport, Ed25519 auth, Opus audio
- egui-test is both lib and binary (TestHarness for programmatic control)
- harness-cli: daemon-based CLI for automated GUI testing
- Build issues: run `cargo build -p egui-test` and fix first error
