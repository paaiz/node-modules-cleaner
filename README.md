# node-modules-cleaner

A fast, interactive Rust CLI tool to scan for `node_modules` directories, view their sizes, and delete them with a single keypress.

## Features

- **Parallel scanning:** Uses multi-threaded directory traversal and size calculation for fast scans across large workspaces.
- **Interactive TUI:** Navigate with arrow keys (`↑`/`↓` or `k`/`j`) and press `Enter` to delete immediately.
- **Smart exclusions:** Skips nested `node_modules` inside `node_modules` as well as `.git`, `target`, `.cargo`, `.rustup`, Python virtual environments, and OS/system directories.
- **Sorted by size:** Largest `node_modules` folders appear at the top.
- **Safe options:** Supports `--list` for dry-run inspection and `--min-size <MB>` to filter small folders.

## Installation

### Option 1: Download Prebuilt Binary (No Rust Required)

Download the latest binary for Windows (`.exe`), macOS, or Linux (`.tar.gz`) from the [Releases](../../releases) page and run it directly (or place it in a folder on your `PATH`).

### Option 2: Install via Cargo (Requires Rust)

```bash
cargo install --git https://github.com/paaiz/node-modules-cleaner
```

Or clone and install locally:

```bash
git clone https://github.com/paaiz/node-modules-cleaner
cd node-modules-cleaner
cargo install --path .
```

## Usage

```bash
# Scan the current directory and open interactive mode
node-modules-cleaner

# Scan a specific folder
node-modules-cleaner --path "C:\Users\you\Projects"

# Only show folders >= 50 MB
node-modules-cleaner --min-size 50

# List results without entering interactive mode
node-modules-cleaner --list

# Delete all found node_modules without confirmation (use with caution)
node-modules-cleaner --delete --yes
```

### Interactive Controls

| Key | Action |
| --- | --- |
| `↑` / `k` | Move selection up |
| `↓` / `j` | Move selection down |
| `Enter` | Prompt to delete the selected `node_modules` folder (press `Enter` again to confirm, or any other key to cancel) |
| `q` / `Esc` / `Ctrl+C` | Exit |

## License

[MIT](LICENSE)

---

Created with ❤️ by @paaiz
