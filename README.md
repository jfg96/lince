# lince

A resident filename index for Linux desktops. lince keeps your home (or any set of folders) in memory, stays current through inotify, and answers name and path searches over a Unix socket in a few milliseconds. It is built for launchers and search overlays that run a query on every keystroke.

```console
$ lince search hypr config
/home/me/.config/hypr
/home/me/.config/hypr/hyprland.conf
…
```

- **Fast:** about 3–4 ms per search on a 314k-entry home, and 12 ms on a million entries (details below).
- **Fresh:** every directory is watched. Changes made before a search are always in its results, and a lost-event overflow is repaired automatically.
- **Accent- and case-insensitive:** `cancion` finds `Canción`, `españa` finds `espana`, and names stored in NFD match too.
- **Name, path and fuzzy passes**, plus filters for kind, extension, hidden files, roots, scopes, depth and gitignore-style excludes.
- **Ranked:** when there are more matches than the client asked for, it keeps the best ones, not whichever came first.
- **Light:** about 40 MB resident for 300k entries, no CPU when idle, and a 5.6 MB snapshot that restarts it in about 120 ms.

Used by [Omarchy Find](https://github.com/jfg96/omarchy-find), which falls back to `fd` when lince is not installed.

## Install

The [releases](https://github.com/jfg96/lince/releases) page has a prebuilt x86-64 binary with its SHA-256:

```sh
tar xzf lince-v0.1.0-x86_64-linux.tar.gz
install -Dm755 lince-v0.1.0-x86_64-linux/lince ~/.local/bin/lince
```

Or build it from source (Rust 1.85+):

```sh
cargo install --locked --git https://github.com/jfg96/lince
```

## Use

```sh
lince serve                      # index ~ and serve $XDG_RUNTIME_DIR/lince/lince.sock
lince search notes pdf           # print matching paths
lince search --dirs --hidden hypr
lince status                     # roots, entries, memory, watches, freshness
```

To run it for the whole session instead of starting it from a client:

```sh
install -Dm644 contrib/systemd/lince.service ~/.config/systemd/user/lince.service
systemctl --user enable --now lince
```

A second `lince serve` on the same socket exits immediately, so clients can always try to start one.

### Configuration

`~/.config/lince/config.json` is optional. Command-line flags (`--root`, `--exclude`, `--socket`, `--snapshot`, `--no-watch`) override it.

```json
{
  "roots": ["~", "/mnt/data"],
  "exclude": [".git", "node_modules", ".cache", "__pycache__", ".venv", "venv", ".local/share/Trash"],
  "watch": true
}
```

By default lince indexes `~` and excludes version-control folders, package caches, virtual environments and the trash. A root inside another root is ignored. Snapshots go to `~/.cache/lince/`, one per configuration.

### Clients

The protocol is one line of JSON per request and per response; see [PROTOCOL.md](PROTOCOL.md). From a shell:

```sh
echo '{"query":"report","kinds":["f"],"extensions":["pdf"],"max_results":20}' | lince query
```

## How it works

- **Compact entries:** each entry is a basename plus a reference to its parent directory (28 bytes). Renaming or moving a directory updates one entry however many descendants it has, and full paths are only rebuilt for the results that are returned.
- **Prepared matching:** names live in one contiguous blob, folded once at index time. Each query term is a single SIMD substring search over that blob, which produces a bitset. Directory bitsets answer "does the path contain this term", and a parallel pass applies the filters. File extensions are interned per entry, so extension filters compare small integers.
- **Crawling:** a parallel walker reads `d_type` and never stats an entry. Each directory gets its inotify watch before it is read, so nothing created during the crawl is missed.
- **Freshness:** all inotify events are applied before each query, and the handlers are idempotent. If the kernel queue overflows, or when loading a snapshot, lince stats every directory and re-reads only those whose ctime changed: creating, deleting or renaming an entry always changes its parent's ctime.
- **Snapshot:** names are front-coded, written atomically on exit and every 10 minutes after changes. It is validated, not trusted, on load.

## Performance

Measured on a 28-thread laptop with btrfs and a warm cache. Each figure is the median over 72 (query, category) cases. The scripts and raw results are in Omarchy Find's `bench/` directory.

| | fd (walk per search) | FSearch 0.3.2 | lince |
|---|---:|---:|---:|
| 314k-entry home: search | 109 ms | 3.3 ms | 3.7 ms |
| home: CPU per search | 1349 ms | 36 ms | 10 ms |
| home: build | — | 650 ms | 110 ms |
| home: resident memory | — | 51–68 MB | 38–50 MB |
| 964k entries: search | 108 ms | 14 ms | 12 ms |
| 964k entries: CPU per search | 1798 ms | 204 ms | 34 ms |

Whenever fd stays under its result cap, lince returns exactly fd's candidates; above the cap, lince keeps the best-ranked ones. FSearch only matches names (no path pass, no fuzzy fallback, no accent folding), so its result sets are smaller.

## Limits

- Linux only (inotify, signalfd). It indexes names, not contents or metadata.
- Watches cost kernel memory (up to about 1 KB each) and count against `fs.inotify.max_user_watches`. A home with 34k directories uses 34k watches. If the limit is reached, responses say `"fresh": false`.
- A cold start right after boot has not been benchmarked yet. The snapshot plus ctime validation is there to keep it short.

## Development

```sh
cargo test --release
bash tests/cli_test.sh               # search semantics through the CLI
python3 tests/stress_updates.py target/release/lince   # live index vs a fresh one after random changes
```

MIT licensed.
