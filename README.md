# lince

A resident filename index for Linux desktops. lince keeps your home (or any set of folders) in memory, stays current through inotify, and answers name and path searches over a Unix socket in a few milliseconds. It is built for launchers and search overlays that run a query on every keystroke.

```console
$ lince search hypr config
/home/me/.config/hypr
/home/me/.config/hypr/hyprland.conf
…
```

- **Fast:** about 1.5 ms per search on a 366k-entry home, and 7 ms on a million entries (details below).
- **Fresh:** every directory is watched. Changes made before a search are always in its results, and a lost-event overflow is repaired automatically.
- **Accent- and case-insensitive:** `cancion` finds `Canción`, `españa` finds `espana`, and names stored in NFD match too.
- **Name, path and fuzzy passes**, plus filters for kind, extension, hidden files, roots, scopes, depth and gitignore-style excludes.
- **Ranked:** when there are more matches than the client asked for, it keeps the best ones, not whichever came first.
- **Light:** about 43 MB resident for 366k entries, no CPU when idle, and a 6.6 MB snapshot that restarts it in about 70 ms.

Used by [Omarchy Find](https://github.com/jfg96/omarchy-find), which fetches a pinned, checksum-verified lince release on first use and falls back to `fd` while lince is unavailable.

## Install

The [releases](https://github.com/jfg96/lince/releases) page has a prebuilt x86-64 binary with its SHA-256:

```sh
tar xzf lince-v0.1.4-x86_64-linux.tar.gz
install -Dm755 lince-v0.1.4-x86_64-linux/lince ~/.local/bin/lince
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
- **Prepared matching:** names live in one contiguous blob, folded once at index time. A name that is already its own folded form (most names in a home) is not stored a second time. Each query term is a single SIMD substring search over that blob, which produces a bitset. Directory bitsets answer "does the path contain this term", and a parallel pass applies the filters. File extensions are interned per entry, so extension filters compare small integers.
- **Narrow queries stay small:** when a term matches few entries, directory hits come from walking the matching subtrees and only the candidates are checked, instead of passing over every entry. A term containing `/` starts from the entries whose name begins with its last part and checks a few ancestors, so no directory path is built. Per-query excludes decide each directory once and check files in parallel.
- **Crawling:** a parallel walker reads `d_type` and never stats an entry. Each directory gets its inotify watch before it is read, so nothing created during the crawl is missed.
- **Freshness:** all inotify events are applied before each query, and the handlers are idempotent. If the kernel queue overflows, or when loading a snapshot, lince stats every directory and re-reads only those whose ctime changed: creating, deleting or renaming an entry always changes its parent's ctime.
- **Snapshot:** names are front-coded, written atomically on exit and every 10 minutes after changes. It is validated, not trusted, on load.

## Performance

Measured on an Intel i7-14700HX laptop (28 logical CPUs), btrfs, warm cache, with the published 0.1.3 and 0.1.4 binaries. lince uses eight query workers by default. The paired 65-case suite runs both on identical frozen indices; times are medians over cases of each case's median engine time:

| | 366k home mirror | 964k synthetic | 4.8M synthetic |
|---|---:|---:|---:|
| All cases, 0.1.3 → 0.1.4 | 3.25 → **1.46 ms** | 9.62 → **7.29 ms** | 43.5 → **31.8 ms** |
| Up to 1,000 matches | 2.88 → **0.89 ms** | 10.13 → **4.49 ms** | 47.8 → **14.5 ms** |
| Over 100,000 matches | 5.64 → 5.91 ms | 12.92 → 11.58 ms | 44.4 → 41.5 ms |
| CPU per search | 9.44 → 5.56 ms | 27.8 → 23.3 ms | 144 → 116 ms |
| Index size | 32.5 → **27.6 MiB** | 75.0 → 69.8 MiB | 376 → 350 MiB |
| Idle RSS after the suite | 45.4 → 42.3 MiB | 100 → 89 MiB | 471 → 428 MiB |
| Ready from snapshot | 99 → 69 ms | 256 → 245 ms | 1164 → 1151 ms |

Every measured response preserves the exact result arrays, order and match counts: **195/195 cases**, plus 1,800 generated requests on the home mirror and the 964k tree. Queries with many matches still pass over the whole index and gain little; the home-mirror difference there is within run-to-run noise. The synthetic trees have far more capitalised and accented names than a real home (34% non-ASCII names, against 0.03% in the mirror), so the index saving on them is smaller.

A separate 60-case query/category benchmark compares FSearch 0.3.2 with lince 0.1.2 (measured 2026-10-14, not repeated since):

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

Changes to the engine are compared with the previous binary on frozen trees: `tests/bench_compare.py` (paired latency, exact results on 65 cases), `tests/parity.py` (exact results on generated requests) and `tests/bench_daemon.py` (startup and memory). See [bench/README.md](bench/README.md) for datasets and commands, and [RELEASING.md](RELEASING.md) for publishing a release.

MIT licensed.
