# cargo-wipe

A keyboard-driven terminal UI for finding and reclaiming disk space consumed
by Cargo `target/` directories across your entire machine.

Rust's incremental build cache is invaluable while you're actively working on
a project, but those `target/` directories accumulate quickly and can easily
consume tens of gigabytes. `cargo-wipe` scans your home directory (or any path
you choose), lists every workspace and standalone package that has something to
reclaim, and lets you delete their build artifacts in a few keystrokes — without
ever leaving the terminal.

---

## Features

- **Full-tree scan** — walks the filesystem from `$HOME` or a directory you
  specify, correctly identifying workspace roots and skipping member crates so
  nothing is listed twice
- **Parallel sizing** — `target/` sizes are measured concurrently, keeping scan
  times short even on large directory trees
- **Sorted by size** — the biggest space hogs appear at the top so you can act
  immediately
- **Multi-select** — pick individual workspaces with `Space`, or select
  everything at once with `a`
- **Safe deletion** — before removing anything, the tool verifies the path is
  non-empty, has a parent, and is literally named `target`; it refuses to touch
  anything that fails this check
- **Live feedback** — a confirmation dialog shows exactly how much space will be
  freed; a results popup reports per-workspace outcomes and flags errors
- **Panic-safe** — the alternate screen and raw mode are restored on both clean
  exit and panic, so your terminal is never left in a broken state

---

## Installation

**From crates.io:**

```sh
cargo install cargo-wipe
```

**From source:**

```sh
git clone https://github.com/markonyango/cargo-wipe
cd cargo-wipe
cargo install --path .
```

Once installed, `cargo-wipe` can be used both as a standalone binary and as a
Cargo subcommand:

```sh
cargo wipe            # scan $HOME
cargo wipe ~/projects # scan a specific directory
cargo-wipe            # same, invoked directly
```

---

## Usage

```
cargo-wipe [START_DIR]
```

| Argument | Default | Description |
|---|---|---|
| `START_DIR` | `$HOME` | Root directory to scan. Falls back to `.` if `$HOME` is unset. |

The tool opens immediately and starts scanning in the background. You can
navigate the list while the scan is still running.

---

## Keybindings

### Main list

| Key | Action |
|---|---|
| `j` / `↓` | Move cursor down |
| `k` / `↑` | Move cursor up |
| `PgDn` | Move cursor down 10 rows |
| `PgUp` | Move cursor up 10 rows |
| `g` / `Home` | Jump to the first workspace |
| `G` / `End` | Jump to the last workspace |
| `Space` | Toggle selection on the highlighted workspace |
| `a` | Select all / deselect all (toggles) |
| `d` | Open the delete confirmation dialog |
| `r` | Rescan the start directory |
| `q` / `Esc` | Quit |
| `Ctrl+C` | Quit (from any mode) |

### Confirmation dialog

| Key | Action |
|---|---|
| `y` | Confirm and delete selected workspaces |
| `n` / `Esc` | Cancel and return to the list |

### Results popup

| Key | Action |
|---|---|
| `Enter` / `Esc` / `Space` | Dismiss and return to the list |
| `r` | Rescan |
| `q` | Quit |

---

## How it works

### Discovery

`cargo-wipe` walks the filesystem in three phases:

1. **Walk** — [`walkdir`](https://docs.rs/walkdir) traverses the directory tree,
   skipping `target/`, `.git/`, `node_modules/`, hidden directories, and other
   VCS cache directories that never contain Cargo projects.

2. **Inspect** — every `Cargo.toml` encountered is read line-by-line to check
   whether it declares a `[workspace]` section, a `[package]` section, or both.
   No external TOML parser is used — top-level section headers always appear on
   their own line, making a line scan accurate enough for root detection.

3. **Root detection** — a manifest is treated as a workspace root when:
   - it declares `[workspace]`, **or**
   - it declares `[package]` and no ancestor directory within the scanned set
     also declares `[workspace]` (i.e. it is not a member crate of a parent
     workspace).

   Member crates are never listed individually; only the workspace root that
   owns them appears in the list.

### Sizing

After discovery, `target/` sizes are computed across all workspace roots
simultaneously using [`rayon`](https://docs.rs/rayon). Workspaces with a
missing or empty `target/` directory are filtered out — there is nothing to
reclaim from them.

Results are sorted by `target/` size, largest first.

### Background scanning

The scan runs on a dedicated thread and sends results to the TUI over an
`mpsc` channel. The interface remains fully interactive while the scan is in
progress, and a spinner indicates that work is ongoing.

### Deletion

`fs::remove_dir_all` is called on the `target/` path of each selected workspace.
Before removing anything, `cargo-wipe` checks that:

- the path is not empty
- the path has a parent directory
- the final path component is exactly `"target"`

Any workspace that fails deletion is reported in the results popup with the
full error message. Workspaces that were deleted successfully are removed from
the list immediately.

---

## Dependencies

| Crate | Purpose |
|---|---|
| [`ratatui`](https://crates.io/crates/ratatui) | TUI rendering |
| [`crossterm`](https://crates.io/crates/crossterm) | Cross-platform terminal I/O |
| [`walkdir`](https://crates.io/crates/walkdir) | Recursive directory traversal |
| [`rayon`](https://crates.io/crates/rayon) | Parallel `target/` size computation |
| [`bytesize`](https://crates.io/crates/bytesize) | Human-readable byte sizes |
| [`anyhow`](https://crates.io/crates/anyhow) | Error handling |

---

## License

MIT — see [LICENSE](LICENSE) for details.
