#!/usr/bin/env python3
"""Reproduces the watch-exhaustion incident at small scale.

Usage: experiments/watches/incident.py OUT_DIR NAME=BIN[:MAX_WATCHES] ...

Each daemon runs in its own user namespace whose inotify watch limit is
LIMIT (/proc/sys/user/max_inotify_watches), so it runs out of watches like
the desktop daemon did at 523k, without touching the system setting. Trees
and state live under OUT_DIR, which must be outside $HOME so the desktop
daemon never indexes them.

Per daemon: status once indexed, query latency, how long a file created in
each of SAMPLE random directories takes to show up, daemon CPU while idle,
the worst query latency while it polls, and whether it is fresh again
after most of the tree is deleted.
"""

from __future__ import annotations

import json
import os
import random
import shutil
import socket
import statistics
import subprocess
import sys
import time

# 10,100 directories and 60,601 entries by default.
TOP, SUB, FILES = (int(os.environ.get(k, d)) for k, d in (("TOP", 100), ("SUB", 100), ("FILES", 5)))
LIMIT = 3000
SAMPLE = 20
SEE_TIMEOUT = 30.0
IDLE_SECONDS = 30
RECOVER_TIMEOUT = 45.0


class Conn:
    def __init__(self, path: str) -> None:
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(10)
        self.sock.connect(path)
        self.file = self.sock.makefile("rb")

    def ask(self, req: dict) -> dict:
        self.sock.sendall((json.dumps(req) + "\n").encode())
        return json.loads(self.file.readline())


def make_tree(root: str) -> None:
    for t in range(TOP):
        for s in range(SUB):
            d = os.path.join(root, f"top{t:03}", f"sub{s:03}")
            os.makedirs(d)
            for f in range(FILES):
                open(os.path.join(d, f"file{f}-{t}-{s}.txt"), "w").close()


def disk_entries(root: str) -> int:
    n = 0
    for _, dirs, files in os.walk(root):
        n += len(dirs) + len(files)
    return n


def cpu_ticks(pid: int) -> int:
    fields = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    return int(fields[11]) + int(fields[12])  # utime + stime


def run(out: str, name: str, binary: str, budget: str | None) -> dict:
    base = os.path.join(out, name)
    tree = os.path.join(base, "tree")
    sock = os.path.join(base, "lince.sock")
    make_tree(tree)
    cmd = [binary, "serve", "--root", tree, "--socket", sock,
           "--snapshot", os.path.join(base, "index.lnc"), "--no-excludes"]
    if budget:
        cmd += ["--max-watches", budget]
    script = f"echo {LIMIT} > /proc/sys/user/max_inotify_watches && exec \"$@\""
    log = open(os.path.join(base, "daemon.log"), "w")
    proc = subprocess.Popen(["unshare", "-Ur", "sh", "-c", script, "sh", *cmd],
                            stdin=subprocess.DEVNULL, stdout=log, stderr=log)
    res: dict = {"name": name, "binary": binary, "budget": budget}
    try:
        t0 = time.monotonic()
        while not os.path.exists(sock):
            if proc.poll() is not None or time.monotonic() - t0 > 120:
                raise RuntimeError(f"{name}: daemon did not start")
            time.sleep(0.05)
        res["ready_s"] = round(time.monotonic() - t0, 2)
        c = Conn(sock)
        st = c.ask({"op": "status"})
        res["start"] = {k: st.get(k) for k in ("entries", "watches", "unwatched", "max_watches", "fresh")}

        lat = []
        for i in range(60):
            t = time.perf_counter()
            r = c.ask({"id": i, "query": f"file{i % 5}-{i}", "max_results": 60})
            lat.append((time.perf_counter() - t) * 1000)
        res["query_ms_p50"] = round(statistics.median(lat[10:]), 2)
        res["fresh_in_answer"] = r.get("fresh")

        # Files created where there may be no watch: how long until visible?
        rng = random.Random(1)
        picks = [(rng.randrange(TOP), rng.randrange(SUB)) for _ in range(SAMPLE)]
        names = []
        for k, (t, s) in enumerate(picks):
            n = f"probe{k}x{t}x{s}"
            open(os.path.join(tree, f"top{t:03}", f"sub{s:03}", n), "w").close()
            names.append(n)
        created = time.monotonic()
        seen: dict[str, float] = {}
        while len(seen) < len(names) and time.monotonic() - created < SEE_TIMEOUT:
            for n in names:
                if n not in seen and c.ask({"id": 1, "query": n})["files"]:
                    seen[n] = time.monotonic() - created
            time.sleep(0.1)
        waits = sorted(seen.values())
        res["probes_seen"] = f"{len(seen)}/{len(names)}"
        res["probe_wait_s_max"] = round(waits[-1], 2) if waits else None

        ticks = cpu_ticks(proc.pid)
        time.sleep(IDLE_SECONDS)
        res["poll_ms"] = c.ask({"op": "status"}).get("poll_ms")
        res["idle_cpu_pct"] = round((cpu_ticks(proc.pid) - ticks) / os.sysconf("SC_CLK_TCK") / IDLE_SECONDS * 100, 2)

        # Queries while it polls: the worst one shows how long a pass stalls.
        lat = []
        end = time.monotonic() + 15
        while time.monotonic() < end:
            t = time.perf_counter()
            c.ask({"id": 3, "query": "file3-7", "max_results": 60})
            lat.append((time.perf_counter() - t) * 1000)
            time.sleep(0.05)
        res["busy_query_ms_p50_max"] = [round(statistics.median(lat), 2), round(max(lat), 2)]

        # Free most watches; a recovering daemon is fresh again.
        for t in range(TOP // 10, TOP):
            shutil.rmtree(os.path.join(tree, f"top{t:03}"))
        freed = time.monotonic()
        res["recovered_s"] = None
        while time.monotonic() - freed < RECOVER_TIMEOUT:
            st = c.ask({"op": "status"})
            if st.get("fresh"):
                res["recovered_s"] = round(time.monotonic() - freed, 2)
                break
            time.sleep(0.25)
        # A change after recovery arrives through events, not polling.
        open(os.path.join(tree, "top000", "sub000", "after.txt"), "w").close()
        res["event_after_recovery"] = bool(c.ask({"id": 2, "query": "after.txt"})["files"])
        st = c.ask({"op": "status"})
        res["end"] = {k: st.get(k) for k in ("entries", "watches", "unwatched", "fresh")}
        res["end"]["disk_entries"] = disk_entries(tree)
    finally:
        proc.terminate()
        try:
            proc.wait(10)
        except subprocess.TimeoutExpired:
            proc.kill()
        log.close()
        shutil.rmtree(tree, ignore_errors=True)
    res["log"] = open(os.path.join(base, "daemon.log")).read().strip().splitlines()
    return res


def main() -> None:
    out = os.path.realpath(sys.argv[1])
    home = os.path.realpath(os.path.expanduser("~"))
    if out == home or out.startswith(home + os.sep):
        sys.exit("OUT_DIR must be outside $HOME: the desktop daemon would index it")
    os.makedirs(out, exist_ok=False)
    results = []
    for spec in sys.argv[2:]:
        name, rest = spec.split("=", 1)
        binary, _, budget = rest.partition(":")
        results.append(run(out, name, os.path.realpath(binary), budget or None))
        print(json.dumps(results[-1], ensure_ascii=False), flush=True)
    json.dump(results, open(os.path.join(out, "results.json"), "w"), indent=1, ensure_ascii=False)


if __name__ == "__main__":
    main()
