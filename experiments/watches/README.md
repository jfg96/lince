# Running out of inotify watches

## Problem

In v0.1.2 the first `ENOSPC` from `inotify_add_watch` set a `degraded` flag
for good: refused directories were never retried or checked again, so their
changes were lost until a restart, and every response said `fresh:false`.
Clients that fall back to walking the disk on `fresh:false` then walk it for
every search. Taking nearly all of `fs.inotify.max_user_watches` also leaves
none for editors and the desktop.

## Change

- `max_watches` caps lince's own watches (default 90% of
  `fs.inotify.max_user_watches`; `--max-watches` or the config file).
- Directories without a watch (over the cap, or refused by the kernel) are
  queued. Round-robin passes of up to 16,384 directories retry the watch and
  re-read those whose ctime changed (`persist::refresh_dirs`). A full cycle
  takes about 10 s; passes are never closer than 50 times their duration
  (about 2% of one core).
- Once every directory has a watch again, `fresh` returns to `true` without
  a restart.
- Status reports `max_watches`, `unwatched` and `poll_ms`; the log says when
  directories go without a watch and when they all have one again.
- A ctime less than a second old is recorded off by one, so the next check
  re-reads the directory (git's racy index). Kernels with multigrain
  timestamps (6.13+) do not show the problem; on older ones a change in the
  same tick as the listing would be missed in a polled directory.

## Reproduction

`incident.py` runs each daemon in its own user namespace with
`/proc/sys/user/max_inotify_watches = 3000`, leaving the system limit alone,
on trees outside `$HOME`:

```bash
python3 -I experiments/watches/incident.py /var/tmp/lince-watch-exp/runN \
  base=OLD_BIN cand=NEW_BIN [cand-budget=NEW_BIN:2000]
TOP=300 SUB=300 FILES=5 python3 -I experiments/watches/incident.py ...   # 540k entries
```

## Results (2026-10-14, x86-64 laptop, kernel 7.2)

60k entries, 10,100 directories, 7,101 without a watch:

| | v0.1.2 | new | new, `--max-watches 2000` |
|---|---|---|---|
| Files created in 20 random directories | 6/20, the rest never | 20/20 within 10 s | 20/20 within 10 s |
| After watches are freed | `fresh:false` for good | `fresh:true` in 9.5 s | `fresh:true` in 9.5 s |
| Entries in index / on disk at the end | 6,010 / 6,013 | 6,013 / 6,013 | 6,013 / 6,013 |
| Idle CPU | 0% | 0.27% | 0.2% |

540k entries, 90,300 directories, 87,301 without a watch:

| | v0.1.2 | new |
|---|---|---|
| Files created in 20 random directories | 0/20 | 20/20 within 22 s (first pass 10 s after start) |
| Entries in index / on disk at the end | 54,030 / 54,033 | 54,033 / 54,033 |
| Polling pass | — | 34 ms (191 ms unchunked) |
| Queries every 50 ms while polling, p50 / worst | 9.1 / 12.2 ms | 8.8 / 43.2 ms |
| Idle CPU | 0% | 2.5% |

v0.1.2 is the release binary built by CI, so its plain query latency is not
comparable one to one with the local build.
