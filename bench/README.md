# Search benchmarks

Measurements from 2026-10-14, Intel Core i7-14700HX (28 logical CPUs), btrfs,
warm filesystem cache. Both engines use release builds with LTO. Default
query budgets are eight workers for lince and 28 for FSearch 0.3.2.
The lince baseline is `bbd853e`; the candidate is `188dcff`.

## Paired lince comparison

`tests/bench_compare.py` gives both daemons copies of the same snapshot.
The 65 cases cover names, paths, fuzzy matching, accents, multiple terms,
browsing, kind/extension/hidden/scope/depth/exclude filters, small result caps,
and `select: first`. Three warmups precede six alternating batches. There are
60 measured requests per case on the 97k/home trees and 30 on the 964k tree.
Each response must preserve the exact result arrays and match counts.

| Entries | Baseline engine | Candidate engine | CPU baseline → candidate |
|---|---:|---:|---:|
| 97,453 | 2.189 ms | 1.786 ms | 7.000 → 5.167 ms |
| 315,143 home mirror | 3.760 ms | 2.997 ms | 11.500 → 8.667 ms |
| 964,036 | 12.733 ms | 9.573 ms | 40.667 → 30.000 ms |

195/195 cases preserve results, order and counts. Watches are disabled on
these frozen trees to isolate query cost; `fresh: false` is expected. Separate
live-update stress checks passed 400 read-after-write checks, 4,000 random
mutations and recovery after 18,384 queued events exceed a 16,384-event limit.

## Operational comparison

The existing [Omarchy Find harness](https://github.com/jfg96/omarchy-find/blob/main/bench/bench_fsearch.py)
measures 12 queries across five categories, with 30 repetitions per case.
FSearch is timed from queueing a search to its completion signal; query-object
construction and rendering result paths are outside that timer. lince's wall
time includes the socket round trip and JSON. CPU is process CPU, not wall time.
The two engines are measured in separate batches. lince watches are enabled;
FSearch monitoring is disabled. No mtimes are requested.

| Dataset | FSearch wall | lince wall | FSearch CPU | lince CPU |
|---|---:|---:|---:|---:|
| Home mirror | 4.280 ms | 2.409 ms | 43.510 ms | 7.000 ms |
| 964k synthetic | 14.901 ms | 9.891 ms | 204.830 ms | 25.500 ms |

These are medians of per-case medians. Matching differs: the FSearch harness
uses automatic path search for terms containing `/`; lince also has path and
fuzzy passes and accent folding. Hidden filters and symlink handling differ.
Consequently, these cases can return different candidate sets.

## Identical-candidate path comparison

`compare_paths.py` uses ten literal one/two-character queries with all,
directory-only and file-only filters. It enables full-path matching in
FSearch, includes hidden entries in lince and uses identical excludes.
Queries occurring in the absolute root prefix are skipped. Such short terms
do not trigger lince's fuzzy pass. The home mirror for this suite omits
symlinks; the synthetic tree contains only regular files and directories.

Four interleaved batches give 60 measured requests per case, rotating engine
order. A separate untimed pass requests every lince candidate and hashes the
sorted, NUL-delimited UTF-8 paths from each engine. All **60/60 candidate sets
and per-kind counts match exactly**. The original and candidate lince binaries
also match each other. Hash extraction is outside timing and RSS measurement.

| Dataset | FSearch wall | lince baseline wall | lince candidate wall | FSearch CPU | Candidate CPU |
|---|---:|---:|---:|---:|---:|
| Home mirror | 9.833 ms | 5.511 ms | 4.052 ms | 148.725 ms | 8.917 ms |
| 964k synthetic | 27.287 ms | 13.561 ms | 12.333 ms | 491.618 ms | 28.250 ms |

The candidate is faster in 52/60 cases. Some directory-only cases favor
FSearch. Identical candidates do not imply identical output work: FSearch
returns all name-sorted references; lince selects up to 2,000 results per kind
by relevance and emits their paths as JSON. These short literal path cases
are a distinct workload from the operational suite; do not mix their medians.

## Reproduction

Create frozen trees with `make_datasets.py` (the mirror copies names into empty
files). Remove symlinks from a dedicated mirror copy for the path comparison.
Use roots such as `/var/tmp/lince-perf/mirror` and `/var/tmp/lince-perf/tree1m`.

```sh
python3 bench/make_datasets.py synth 1000000 /var/tmp/lince-perf/tree1m
python3 bench/make_datasets.py mirror "$HOME" /var/tmp/lince-perf/mirror
python3 tests/bench_compare.py ROOT BASELINE_BIN CANDIDATE_BIN paired.json --repeats 10
```

To build `fsbench.c`, install the normal FSearch build dependencies (Meson,
Ninja, GTK3/GLib development tools, PCRE2, ICU and documentation tools), then:

```sh
git clone --depth 1 --branch 0.3.2 https://github.com/cboxdoerfer/fsearch.git /var/tmp/fsearch-bench
cp bench/fsbench.c /var/tmp/fsearch-bench/src/tests/fsbench.c
printf "\nexecutable('fsbench', 'fsbench.c', dependencies: libfsearch_dep)\n" >> /var/tmp/fsearch-bench/src/tests/meson.build
meson setup /var/tmp/fsearch-bench/build /var/tmp/fsearch-bench -Dbuildtype=release -Db_lto=true
ninja -C /var/tmp/fsearch-bench/build src/tests/fsbench
python3 bench/compare_paths.py ROOT /var/tmp/fsearch-bench/build/src/tests/fsbench BASELINE_BIN CANDIDATE_BIN paths.json --repeats 15
```

The helper uses engine APIs without changing the engine. `FSBENCH_FULL_PATH`
selects path matching; `FSBENCH_HASH` enables post-timing candidate hashes.
Without either variable it supports the operational harness's existing CLI.
A busy response while retrieving validation paths is retried outside timing.

## Recorded data and limits

[Summary](results/summary.json) · [Manifest and archive checksums](results/manifest.json).
`results/*.json.gz` contains per-case measurements, counts, phase timings,
available samples, candidate hashes and binary checksums. Decompress with
`gzip -dc`. Dataset paths in metadata are replaced by labels and hostnames are
removed; candidate hashes retain the originally measured path bytes. The
private mirror itself is not distributed. Synthetic generation is deterministic.

CPU for lince is averaged from `/proc` ticks; FSearch uses `getrusage`.
The laptop is not frequency-pinned. Small differences can be noise, and cold
starts, GUI rendering and steady-state monitor overhead are not measured by
these query suites. The operational data archives have per-case aggregates;
paired and path archives additionally contain individual lince timing samples.
