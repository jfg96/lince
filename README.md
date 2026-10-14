# lince

A resident filename index for Linux desktops. lince keeps your home (or any set of folders) in memory, stays current through inotify, and answers name and path searches over a Unix socket in a few milliseconds. It is built for launchers and search overlays that run a query on every keystroke.

```console
$ lince search hypr config
/home/me/.config/hypr
/home/me/.config/hypr/hyprland.conf
…
```

- **Fast:** about 3 ms per search on a 315k-entry home, and 10 ms on a million entries (details below).
- **Fresh:** every directory is watched. Changes made before a search are always in its results, and a lost-event overflow is repaired automatically.
- **Accent- and case-insensitive:** `cancion` finds `Canción`, `españa` finds `espana`, and names stored in NFD match too.
- **Name, path and fuzzy passes**, plus filters for kind, extension, hidden files, roots, scopes, depth and gitignore-style excludes.
- **Ranked:** when there are more matches than the client asked for, it keeps the best ones, not whichever came first.
- **Light:** about 40 MB resident for 300k entries, no CPU when idle, and a 5.6 MB snapshot that restarts it in about 120 ms.

Used by [Omarchy Find](https://github.com/jfg96/omarchy-find), which fetches a pinned, checksum-verified lince release on first use and falls back to `fd` while lince is unavailable.

## Install

The [releases](https://github.com/jfg96/lince/releases) page has a prebuilt x86-64 binary with its SHA-256:

```sh
tar xzf lince-v0.1.3-x86_64-linux.tar.gz
install -Dm755 lince-v0.1.3-x86_64-linux/lince ~/.local/bin/lince
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

A second `lince serve` on the same socket exits immediately, so clients can always try to start one. A client that starts the daemon itself can pass `--exit-with-parent` and keep the daemon's stdin open: lince exits when it reads end-of-file, so it never outlives the client.

### Configuration

`~/.config/lince/config.json` is optional. Command-line flags (`--root`, `--exclude`, `--socket`, `--snapshot`, `--no-watch`, `--max-watches`) override it.

```json
{
  "roots": ["~", "/mnt/data"],
  "exclude": [".git", "node_modules", ".cache", "__pycache__", ".venv", "venv", ".local/share/Trash"],
  "watch": true,
  "max_watches": 100000
}
```

`max_watches` caps the inotify watches lince takes; by default 90% of `fs.inotify.max_user_watches`, so editors and the desktop keep some.

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

Measured on an Intel i7-14700HX laptop (28 logical CPUs), btrfs, warm cache. lince uses eight query workers by default. The paired 65-case suite compares the current engine with commit `bbd853e` (0.1.1) on identical frozen indices:

| Entries | Previous engine | Current engine | CPU per search (previous → current) |
|---|---:|---:|---:|
| 97,453 synthetic | 2.19 ms | **1.79 ms** | 7.00 → 5.17 ms |
| 315,143 home mirror | 3.76 ms | **3.00 ms** | 11.50 → 8.67 ms |
| 964,036 synthetic | 12.73 ms | **9.57 ms** | 40.67 → 30.00 ms |

Every measured response preserves the exact result arrays and match counts: **195/195 cases**. Workers are reused across requests, blob hits reuse an owner cursor, and boundary ties are partitioned instead of fully sorted. The protocol and snapshot format are unchanged.

A separate 60-case query/category benchmark compares FSearch 0.3.2 with the current lince engine:

| | FSearch | lince, including socket and JSON |
|---|---:|---:|
| 315k home mirror: search | 4.28 ms | **2.41 ms** |
| home: CPU per search | 43.51 ms | **7.00 ms** |
| 964k synthetic: search | 14.90 ms | **9.89 ms** |
| synthetic: CPU per search | 204.83 ms | **25.50 ms** |

These operational cases use each engine's matching behavior and can return different candidates. A separate full-path suite verifies identical candidate sets in 60/60 cases; its medians are 9.83 vs 4.05 ms on the home mirror and 27.29 vs 12.33 ms on the synthetic tree. FSearch uses 28 workers and returns name-sorted references; lince selects up to 2,000 results per kind by relevance and emits paths as JSON. Some directory-only queries remain faster with FSearch.

[Benchmark methods, reproduction commands and per-case data](bench/README.md). The earlier comparison with the fd walk remains available in [Omarchy Find's benchmarks](https://github.com/jfg96/omarchy-find/tree/main/bench).

## Limits

- Linux only (inotify, signalfd). It indexes names, not contents or metadata.
- Watches cost kernel memory (up to about 1 KB each) and count against `fs.inotify.max_user_watches`. A home with 34k directories uses 34k watches. Directories past `max_watches`, or refused by the kernel, are polled by ctime in round-robin passes of at most 16k directories (tens of milliseconds each, about 2% of one core), every directory about every 10 seconds or more, and watched again once watches are freed. Meanwhile responses say `"fresh": false` and status reports `unwatched`.
- A cold start right after boot has not been benchmarked yet. The snapshot plus ctime validation is there to keep it short.

## Development

```sh
cargo test --release
bash tests/cli_test.sh               # search semantics through the CLI
python3 tests/stress_updates.py target/release/lince   # live index vs a fresh one after random changes
```

MIT licensed.
