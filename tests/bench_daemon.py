#!/usr/bin/env python3
"""Startup, memory and latency of lince daemons on one frozen tree.

Usage: bench_daemon.py ROOT OUT.json BIN [BIN...] [--repeats 5]

For each binary, in turn: a cold start (crawl), the query suite of
tests/bench_compare.py, then a restart from the snapshot it saved and the
suite again. Recorded per start:
  ready_ms       from launch until the first answer (the socket is bound
                 only once the index is ready);
  index_bytes    the daemon's own estimate of its arrays;
  rss ready      VmRSS right after startup;
  rss after      VmRSS right after the suite, before any trim;
  rss idle       VmRSS 6.5 s later (the daemon trims its heap after 5 s idle);
  peak           VmHWM;
  latency        median over cases of each case's median wall and engine
                 time, the slowest case, and process CPU per search.
Watches are disabled, as in bench_compare.py. Run the binaries on the same
tree in one invocation; unlike bench_compare.py they are not interleaved,
so compare latency there and use this for startup and memory.
"""
import argparse
import json
import os
import statistics
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench_compare import Daemon, cases  # noqa: E402


def memory(pid):
    out = {}
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        key, _, value = line.partition(":")
        if key in ("VmRSS", "VmHWM"):
            out[key] = int(value.split()[0])
    return out


def suite(d, repeats):
    rows = []
    cpu0 = d.cpu()
    for req in cases():
        for _ in range(2):
            d.ask(req)
        walls, engines = [], []
        for _ in range(repeats):
            wall, r = d.ask(req)
            walls.append(wall)
            engines.append(r["engine_us"] / 1000)
        rows.append({"request": req, "matches": r["matched_dirs"] + r["matched_files"],
                     "wall_ms": statistics.median(walls), "engine_ms": statistics.median(engines)})
    return {"median_wall_ms": statistics.median(r["wall_ms"] for r in rows),
            "median_engine_ms": statistics.median(r["engine_ms"] for r in rows),
            "slowest_engine_ms": max(r["engine_ms"] for r in rows),
            "cpu_ms_per_search": (d.cpu() - cpu0) / (len(rows) * repeats),
            "cases": rows}


def measure(binary, root, repeats):
    out = {"binary": str(binary)}
    with tempfile.TemporaryDirectory(prefix="lince-bench-") as tmp:
        for start in ("cold", "snapshot"):
            t0 = time.perf_counter()
            d = Daemon(binary, root, Path(tmp), "daemon",
                       seed=Path(tmp) / "kept.lnc" if start == "snapshot" else None)
            try:
                status = d.ask({"op": "status"})[1]
                res = {"ready_ms": (time.perf_counter() - t0) * 1000, "entries": status["entries"],
                       "index_bytes": status["index_bytes"], "rss_ready_kib": memory(d.proc.pid)["VmRSS"]}
                res["queries"] = suite(d, repeats)
                after = memory(d.proc.pid)
                time.sleep(6.5)
                res.update(rss_after_kib=after["VmRSS"], rss_idle_kib=memory(d.proc.pid)["VmRSS"],
                           peak_kib=memory(d.proc.pid)["VmHWM"])
            finally:
                d.close()
            # The daemon saved its snapshot on exit; the next start loads it.
            d.snapshot.replace(Path(tmp) / "kept.lnc")
            out[start] = res
            q = res["queries"]
            print(f"{Path(binary).name:28} {start:8} ready {res['ready_ms']:7.0f} ms  "
                  f"index {res['index_bytes'] / 2**20:6.1f} MiB  RSS ready {res['rss_ready_kib'] / 1024:6.1f}  "
                  f"idle {res['rss_idle_kib'] / 1024:6.1f}  peak {res['peak_kib'] / 1024:6.1f} MiB  "
                  f"engine p50 {q['median_engine_ms']:6.2f} ms  slowest {q['slowest_engine_ms']:6.1f}  "
                  f"cpu/search {q['cpu_ms_per_search']:6.2f} ms", flush=True)
    return out


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("root")
    p.add_argument("output")
    p.add_argument("binaries", nargs="+")
    p.add_argument("--repeats", type=int, default=5)
    a = p.parse_args()
    root = str(Path(a.root).resolve())
    result = {"root": root, "repeats_per_case": a.repeats,
              "platform": {"system": os.uname().sysname, "release": os.uname().release,
                           "machine": os.uname().machine, "cpu_count": os.cpu_count(),
                           "LINCE_THREADS": os.environ.get("LINCE_THREADS", "default")},
              "runs": [measure(b, root, a.repeats) for b in a.binaries]}
    Path(a.output).write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
