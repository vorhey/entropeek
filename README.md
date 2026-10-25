# entropeek

A fast terminal UI to **browse, preview, resume and delete** your coding-agent
sessions. Supports **Claude Code** and **Codex**.

Inspired by [showagent](https://github.com/aytzey/showagent), without the
cross-agent hand-off, and built to stay instant with thousands of sessions.
It uses showagent's look and colors (light and dark), so the two feel alike.

```
entropeek  5 sessions   1 claude 3   2 codex 2
╭─ Sessions ───────────────────────────────────────────────────────────────────────────────────╮
│  AGENT    UPDATED  TITLE                                                                     │
│▾ ~/code/api (2)                                                                              │
│❯  CLAUDE  3h ago   Rate limiting for the public API                                          │
│   CODEX   5h ago   Review rate limiter                                                       │
│▾ ~/code/site (2)                                                                             │
│   CLAUDE  1d ago   Responsive landing page                                                   │
│   CODEX   3d ago   Slow build investigation                                                  │
╰──────────────────────────────────────────────────────────────────────────────────────────────╯
╭─ Conversation ───────────────────────────────────────────────────────────────────────────────╮
│ Rate limiting for the public API                                                             │
│ workspace  ~/code/api · ⎇ main                                                               │
│ updated    2026-10-24 15:40 (3h ago) · 1 prompt · 655 B                                      │
│ session     CLAUDE  6f1c2a90-1111-4c1e-9d1a-000000000001                                     │
│ command    cd ~/code/api && claude --resume 6f1c2a90-1111-4c1e-9d1a-000000000001             │
│ ──────────────────────────────────────────────────────────────────────────────────────────── │
│ ▍You  2026-10-24 15:40                                                                       │
│   add rate limiting to the public endpoints                                                  │
╰────────────────────────────────────────────────────────────────────────────────────────── 0% ╯
╭─ Keys ───────────────────────────────────────────────────────────────────────────────────────╮
│ SESSION                VIEW                   FIND                   MORE                    │
│ enter resume           tab focus preview      /     search           t theme dark            │
│ d     delete           z   full screen        1-2   agents           ? hide keys             │
│ u     undo             p   preview on         space collapse         q quit                  │
│ y     copy command     w   groups on          r     rescan           i icons off             │
╰──────────────────────────────────────────────────────────────────────────────────────────────╯
```

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/vorhey/entropeek/main/scripts/install.sh | sh
# or: brew install vorhey/tap/entropeek
entropeek
```

The script installs the latest release for Linux or macOS (x86_64 or arm64)
into `~/.local/bin`, after checking it against the release's `SHA256SUMS`.
`ENTROPEEK_INSTALL_DIR` picks another directory and `ENTROPEEK_VERSION` a
specific release (e.g. `v0.1.0`). Linux builds are static, so they run on any
distribution.

From source (Rust 1.88 or newer):

```sh
cargo install --git https://github.com/vorhey/entropeek
```

## Use

```sh
entropeek                     # interactive browser (lists when not in a terminal)
entropeek list                # table of sessions, newest first
entropeek list --json         # machine-readable
entropeek list -q "rate limit" --agent claude -l 10
entropeek show latest         # print a conversation (--json, --last N)
entropeek resume <id|latest>  # reopen in its agent (--print to just show the command)
entropeek delete <id>         # move to the trash (asks first; --yes to skip)
entropeek trash               # list deleted sessions
entropeek trash restore <id>  # put one back
entropeek trash purge --older-than-days 30   # or --all
```

### Theme

Like Claude Code, entropeek matches your terminal: at startup it asks the
terminal for its background color and picks the light or dark theme. If the
terminal doesn't say, it falls back to `$COLORFGBG`, then dark. Override it
with `--theme light|dark|auto` or `ENTROPEEK_THEME`, or press `t` while
running.

### Icons

With a [Nerd Font](https://www.nerdfonts.com/), entropeek shows icons for
agents, workspaces, branches and messages. No terminal reports its font, so
entropeek works it out at startup, cheapest evidence first:

1. `--icons on|off|auto` / `ENTROPEEK_ICONS`, then the common `NERD_FONT=1|0`.
2. Which terminal this is: environment variables, the terminal's version
   answer (works over SSH) and, inside tmux, tmux's client terminal.
3. kitty 0.36+, WezTerm and Ghostty have Nerd Font symbols built in: icons on.
4. Otherwise the font decides: kitty reports it; for Alacritty, foot, Konsole
   and VS Code it is read from their settings.
5. Anything else: no icons (a missing icon beats a broken glyph).

The questions to the terminal go out together with the background color
query, and reading configuration files (or asking tmux) happens on a
worker: if that is slow, the list appears without icons and they follow.
Press `i` to switch icons; the status line also says what the detection
decided and why.

A session can be named by its id, any unique id prefix of 4+ characters,
`claude:<id>` / `codex:<id>` when two agents use the same id, or the path
of its file. If a name matches more than one session, entropeek says so
instead of picking one. `delete` never accepts `latest`.

### Keys

| Key | Action |
|---|---|
| `j`/`k`, `↑`/`↓`, `g`/`G`, `PgUp`/`PgDn`, `^u`/`^d` | move |
| `enter` | resume the session (on a workspace header: fold it) |
| `/` | search titles, prompts, paths, ids, branches (all words must match) |
| `esc` | clear search, cancel, go back |
| `d` then `y` | move the session to the trash (any other key cancels; holding `d` never deletes) |
| `u` | undo the last delete |
| `space`, `h`/`l` | fold / unfold a workspace |
| `w` | group by workspace on/off |
| `1`–`9` | show / hide an agent |
| `tab` | focus the preview (`j`/`k` scroll, `g`/`G` top/end); full screen if there is no room for it |
| `z` | full-screen transcript |
| `p` | preview pane on/off |
| `y` | copy the resume command (OSC 52 clipboard) |
| `t` | switch between the light and dark theme |
| `i` | Nerd Font icons on / off |
| `r` | rescan |
| `?` | hide / show the keys box (on small terminals: the full help) |
| `q`, `^c` | quit |

## What it reads and writes

| Agent | Sessions | Titles | Resume |
|---|---|---|---|
| Claude Code | `~/.claude/projects/<dir>/<id>.jsonl` (`$CLAUDE_CONFIG_DIR`) | `/rename` title, else the generated title | `claude --resume <id>` in the session's folder |
| Codex | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (`$CODEX_HOME`) | thread name from `session_index.jsonl` | `codex resume <id>` in the session's folder |

- **Read-only** except for deletes. Agent files are never modified.
- **Delete moves to a trash** at `~/.local/share/entropeek/trash` (`$ENTROPEEK_DATA_DIR`).
  For Claude it also takes the session's subagent folder, `session-env/<id>`,
  `file-history/<id>` and todos. The trash is built so files are not lost:
  - Each entry's `manifest.json` records where every file came from, and is
    written and flushed to disk *before* anything moves. A delete or restore
    that was interrupted (a crash or kill) shows up in `entropeek trash` as
    interrupted; `trash restore` finishes it. Directory changes are flushed
    as they happen, but whether they all survive a sudden power loss also
    depends on the filesystem.
  - Moves never overwrite: if a file reappears where a session is being
    restored, the restore stops and everything stays in the trash. On a
    filesystem that cannot rename without replacing, the destination is
    claimed first (a hard link or an empty directory), or the move is refused.
  - Each delete, restore and purge locks its entry, so a second entropeek
    (say `trash purge --all` in another terminal) skips or refuses an entry
    that is still being filled or emptied.
  - Sessions that another session needs are not deleted: deleting a Codex
    session that a fork refers to (instead of copying its history) is
    refused until the fork is gone, and a delete whose related files cannot
    all be checked stops instead of leaving some behind.
  - If a delete fails halfway and something cannot be put back, it stays in
    the trash as a restorable entry instead of being thrown away.
  - Only paths that really resolve inside the agent's directory are touched.
    A linked agent directory (`~/.claude -> /data/claude`) is fine; a project
    folder that links outside it is listed but cannot be deleted. Moves are
    made relative to directories opened without following links, so one
    swapped for a link after the check is refused too.
  - Across filesystems, items are copied to a hidden, private staging name
    (keeping links, permissions and modification times), checked to be
    unchanged, flushed, and only then given their real name and removed from
    where they were. Special files are refused, and if anything in the item
    changed during the copy, the original is left alone. An interrupted copy
    never blocks a later `trash restore`. (A program still appending through
    a file it already has open can lose what it writes after the final
    check; quit the agent before deleting its live session.)
  - Quitting or resuming waits for a running delete or restore to finish.
  - The files of an entry are plain files in `<entry>/items/<n>`, so they can
    always be recovered by hand using the manifest.
- **Cache** at `~/.cache/entropeek/index.bin` (`$ENTROPEEK_CACHE_DIR`): parsed
  metadata keyed by file path, modification time and size. A file that cannot
  be read is reported and retried, never remembered as empty. `--no-cache`
  ignores the cache.
- entropeek's own directories are kept private to you (`0700`, tightened if
  they already existed) and its files are written atomically as new `0600`
  files. Trashed files keep their own permissions inside the private trash.
- Relative `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `ENTROPEEK_*` values are resolved
  once against the directory entropeek starts in. Resume runs the agent
  program found on `PATH` at that moment, by its absolute path; printed and
  copied commands use that path too when `PATH` found it through a relative
  entry. Session ids that look like options (`--last`) are refused.
- Text from session files (titles, paths, ids) is cleaned of terminal control
  characters before it is printed.

## Why it's fast

- **Nothing slow runs on the input thread.** Loading the cache, scanning,
  reading and wrapping transcripts, checking a resume command, deletes and
  restores all run on a fixed set of worker threads, and large wrapped
  transcripts are freed on a background thread. Repeated rescans collapse
  into one; only the newest preview request runs. Keys typed while the
  terminal answers its startup questions are kept.
- **Cache + background refresh.** The list appears from the cache immediately; a
  background scan re-parses only files whose mtime or size changed.
- **Cheap parsing.** Lines are split with SIMD `memchr` and skipped by a byte
  check before any JSON parse; tool results (most of the bytes) are rarely
  parsed for the list. Files are parsed in parallel with `rayon`.
- **Only visible rows are drawn**, search runs over pre-lowercased text,
  counts are kept up to date instead of recomputed per frame, and wrapped
  transcripts are cached (within a memory budget). A preview holds at most
  100,000 lines (the newest; `entropeek show` prints everything), so its
  size is bounded however the text is shaped.
- **Input is handled in bounded batches**: a long paste arrives as one paste
  (bracketed paste) and is filtered once; outside the search box it does
  nothing. The screen updates at least every frame even under a flood of
  keys.

`cargo test --release -- --ignored --nocapture` measures worst cases, not
just averages. On a desktop Linux machine (2026), with fixtures freshly
written (so the OS file cache is warm):

| Operation | Typical | Worst seen |
|---|---:|---:|
| Frame with 10,000 sessions | 0.34 ms | 0.5 ms |
| Search keystroke over 10,000 sessions + frame | 0.7 ms | 1.2 ms |
| Frame while a 4 MB transcript is being wrapped | 0.4 ms | 1.3 ms |
| Taking a finished 100,000-line wrap (replacing the last) | < 0.01 ms | 0.05 ms |
| Longest event-loop pause while a 21 MB session loads | | 2 ms |
| Scan 2,000 files (280 MB), no metadata cache | 28 ms | |
| Same, with the metadata cache | 3 ms | 3.5 ms |

Wrapping itself (about 50 ms for 4 MB) happens on a worker. Previews show at
most the newest 16 MB of a conversation; `entropeek show` prints all of it.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for the checks every change passes and
the commit message convention ([Conventional Commits](https://www.conventionalcommits.org/)).

```sh
cargo test                                            # unit + end-to-end tests
cargo test --release -- --ignored --nocapture         # load and latency tests
cargo clippy --all-targets -- -D warnings
cargo run --release --example fake_home -- /tmp/fake  # safe sample data to play with
```

Point entropeek at the sample data with:

```sh
HOME=/tmp/fake CLAUDE_CONFIG_DIR=/tmp/fake/.claude CODEX_HOME=/tmp/fake/.codex \
  ENTROPEEK_CACHE_DIR=/tmp/fake/.cache ENTROPEEK_DATA_DIR=/tmp/fake/.data entropeek
```

### Layout

| Path | What |
|---|---|
| `src/provider/` | one file per agent (`claude.rs`, `codex.rs`) implementing `Provider` |
| `src/index.rs`, `src/cache.rs` | parallel scan with the mtime/size cache |
| `src/trash.rs`, `src/actions.rs` | recoverable delete, restore, resume |
| `src/fsutil.rs` | no-overwrite renames, private atomic writes, real-path checks |
| `src/tui/app.rs` | all UI state and key handling, no terminal needed (most tests live here) |
| `src/tui/view.rs` | drawing, tested against ratatui's `TestBackend` |
| `src/tui/mod.rs` | terminal setup and the event loop |
| `src/tui/jobs.rs` | worker threads for scans, previews, wrapping, resume checks, deletes |
| `src/tui/theme.rs` | light and dark palettes (showagent's colors) |
| `src/tui/terminal.rs` | the startup query: background color, terminal version, kitty's font |
| `src/tui/icons.rs` | Nerd Font glyphs and deciding whether the terminal can show them |
| `src/testkit.rs` | builders for realistic Claude/Codex session files |
| `tests/cli.rs` | end-to-end tests running the binary on a fake home |

To add an agent: implement `Provider` in `src/provider/<agent>.rs`, add it to
`AgentKind` and `provider::all`, and add fixtures to `testkit`.

## License

MIT. See [LICENSE](LICENSE).
