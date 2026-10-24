# Contributing

## Before you commit

Every change keeps these green (CI runs them on Linux and macOS):

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --release -- --ignored     # load and latency tests
```

New behavior comes with tests, and bug fixes come with a test that fails
without the fix. `cargo run --release --example fake_home -- /tmp/fake`
creates sample data, so deletes can be tried without touching real sessions
(see the README).

## Commit messages

entropeek follows [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/):

```
<type>(<scope>): <summary>

<body: what changed and why, wrapped at 72 columns>

<footer: BREAKING CHANGE: ..., Fixes #123>
```

- **summary**: imperative, lowercase, no period, at most ~72 characters
  ("add", not "added" or "adds").
- **scope** is optional; use it when the change lives in one area.
- **body** explains *why* when that is not obvious from the summary.

### Types

| Type | For |
|---|---|
| `feat` | a new feature or user-visible behavior |
| `fix` | a bug fix |
| `perf` | a change that makes something faster or lighter |
| `refactor` | restructuring that changes no behavior |
| `test` | adding or fixing tests only |
| `docs` | README, CONTRIBUTING, code comments |
| `style` | formatting only (`cargo fmt`) |
| `build` | `Cargo.toml`, dependencies, release profile |
| `ci` | GitHub Actions workflows |
| `chore` | anything else that touches no source or tests |

### Scopes

Match the code layout:

| Scope | Area |
|---|---|
| `tui` | the interactive browser (`src/tui/`) |
| `theme` | colors and light/dark detection |
| `icons` | Nerd Font icons and their detection |
| `cli` | commands and their output |
| `claude`, `codex` | an agent's parser (`src/provider/`) |
| `index`, `cache` | scanning and the session cache |
| `trash` | delete, restore and purge |
| `search` | search and session selectors |

### Breaking changes

Add `!` after the type or scope and explain it in a `BREAKING CHANGE:`
footer. Examples: a changed key binding, a removed flag, or a different JSON
output.

```
feat(tui)!: confirm deletes with y instead of a second d

BREAKING CHANGE: `dd` no longer deletes; press `d` then `y`.
```

### Examples

```
feat(codex): hide sub-agent threads and delete them with their session
fix(trash): keep files in the trash when a rollback cannot put them back
perf(tui): wrap long transcripts on a worker thread
docs: explain icon detection in the README
ci: check the minimum supported Rust version
```

## Releasing

1. Bump `version` in `Cargo.toml`, run `cargo build` so `Cargo.lock`
   follows, and commit: `chore(release): v0.2.0`.
2. Tag and push:

   ```sh
   git tag v0.2.0
   git push origin main v0.2.0
   ```

The [release workflow](.github/workflows/release.yml) checks that the tag
matches `Cargo.toml`, runs the checks, builds static Linux binaries (x86_64,
arm64) and macOS binaries (Intel, Apple Silicon), and publishes a GitHub
release with `SHA256SUMS`. That is what `scripts/install.sh` downloads.

### Homebrew

`brew install vorhey/tap/entropeek` reads `Formula/entropeek.rb` from the
[vorhey/homebrew-tap](https://github.com/vorhey/homebrew-tap) repository. The
release workflow writes it (with `scripts/homebrew-formula.sh`) when the
`HOMEBREW_TAP_TOKEN` secret is set: a fine-grained token with
*Contents: read and write* on `vorhey/homebrew-tap` only. Without the
secret, the formula is still attached to each release as `entropeek.rb` to
copy by hand.
