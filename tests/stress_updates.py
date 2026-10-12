#!/usr/bin/env python3
"""Incremental-update stress test for a running `lince serve`.

Usage: tests/stress_updates.py LINCE_BIN [STEPS] [SEED]

Starts a daemon on a scratch tree, then checks:
  * read-your-writes: a change made before a query is in its answer;
  * random operations (files and directories created, removed, renamed and
    moved inside, into and out of the tree, renamed to excluded or hidden
    names, freshly created directories populated at once): afterwards the
    live index must equal a fresh one-shot index of the same tree;
  * an inotify queue overflow (daemon frozen with SIGSTOP) is recovered.
Exits non-zero on any mismatch.
"""

from __future__ import annotations

import json
import os
import random
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

EXCLUDES = [".git", "node_modules"]


class Conn:
    def __init__(self, path: str) -> None:
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(path)
        self.file = self.sock.makefile("rb")

    def ask(self, req: dict) -> dict:
        self.sock.sendall((json.dumps(req) + "\n").encode())
        return json.loads(self.file.readline())


def whole_index(conn: Conn) -> set[str]:
    r = conn.ask({"query": "", "hidden": True, "max_results": 10**8, "select": "first", "exclude": EXCLUDES})
    return set(r["dirs"]) | set(r["files"])


def fresh(binary: str, root: str) -> set[str]:
    out = set()
    for kind in ("d", "f"):
        argv = [binary, "oneshot", "--root", root, "--query", "", "--kind", kind, "--hidden", "--max-results", "100000000"]
        for e in EXCLUDES:
            argv += ["--exclude", e]
        out |= {l for l in subprocess.run(argv, capture_output=True, text=True, check=True).stdout.splitlines() if l}
    return out


def main() -> int:
    binary = os.path.abspath(sys.argv[1])
    steps = int(sys.argv[2]) if len(sys.argv) > 2 else 4000
    rng = random.Random(int(sys.argv[3]) if len(sys.argv) > 3 else 7)
    work = tempfile.mkdtemp(prefix="lince-stress-")
    root, outside = os.path.join(work, "root"), os.path.join(work, "outside")
    os.makedirs(outside)
    for i in range(50):
        os.makedirs(os.path.join(root, f"d{i}", "sub"))
        for j in range(10):
            open(os.path.join(root, f"d{i}", f"f{j}.txt"), "w").close()
    sock = os.path.join(work, "lince.sock")
    argv = [binary, "serve", "--socket", sock, "--root", root, "--snapshot", os.path.join(work, "snap.lnc")]
    for e in EXCLUDES:
        argv += ["--exclude", e]
    proc = subprocess.Popen(argv, stderr=subprocess.DEVNULL)
    failures = []
    try:
        for _ in range(500):
            try:
                conn = Conn(sock)
                break
            except OSError:
                time.sleep(0.01)
        else:
            raise SystemExit("daemon did not start")

        stale = 0
        for i in range(200):
            path = os.path.join(root, f"d{i % 50}", f"ryw_{i}_zq.md")
            open(path, "w").close()
            stale += path not in conn.ask({"query": f"ryw_{i}_zq", "hidden": True})["files"]
            os.unlink(path)
            stale += path in conn.ask({"query": f"ryw_{i}_zq", "hidden": True})["files"]
        print(f"read-your-writes: {stale} stale answers in 400 checks")
        if stale:
            failures.append("read-your-writes")

        dirs = [os.path.join(root, f"d{i}") for i in range(50)]

        def any_dir() -> str:
            live = [d for d in dirs if os.path.isdir(d)]
            return rng.choice(live) if live else root

        for step in range(steps):
            op = rng.choice(["mkfile"] * 4 + ["mkdir"] * 3 + ["rm", "rename", "rename", "move_dir",
                            "move_out", "move_in", "move_in", "to_excluded", "to_hidden", "rmtree"])
            try:
                if op == "mkfile":
                    open(os.path.join(any_dir(), f"s{step}.dat"), "w").close()
                elif op == "mkdir":
                    d = os.path.join(any_dir(), f"n{step}")
                    os.makedirs(os.path.join(d, "x", "y"))
                    for k in range(3):
                        open(os.path.join(d, "x", "y", f"k{k}"), "w").close()
                    dirs.append(d)
                elif op in ("rm", "rename"):
                    base = any_dir()
                    files = [f for f in os.listdir(base) if os.path.isfile(os.path.join(base, f))]
                    if files:
                        f = os.path.join(base, rng.choice(files))
                        if op == "rm":
                            os.unlink(f)
                        else:
                            os.rename(f, os.path.join(any_dir(), f"r{step}_" + os.path.basename(f)))
                elif op == "move_dir":
                    src, dst = any_dir(), any_dir()
                    if src != root and not dst.startswith(src):
                        target = os.path.join(dst, f"m{step}")
                        os.rename(src, target)
                        dirs = [target + d[len(src):] if d == src or d.startswith(src + "/") else d for d in dirs]
                elif op == "move_out":
                    src = any_dir()
                    if src != root and rng.random() < 0.5:
                        os.rename(src, os.path.join(outside, f"o{step}"))
                elif op == "move_in":
                    kids = os.listdir(outside)
                    if kids:
                        target = os.path.join(any_dir(), f"i{step}")
                        os.rename(os.path.join(outside, rng.choice(kids)), target)
                        dirs.append(target)
                elif op == "to_excluded":
                    src = any_dir()
                    if src != root:
                        os.rename(src, os.path.join(os.path.dirname(src), "node_modules"))
                elif op == "to_hidden":
                    src = any_dir()
                    if src != root:
                        target = os.path.join(os.path.dirname(src), f".h{step}")
                        os.rename(src, target)
                        dirs.append(target)
                elif op == "rmtree":
                    src = any_dir()
                    if src != root and rng.random() < 0.15:
                        shutil.rmtree(src)
            except OSError:
                pass
            if step % 500 == 0:
                whole_index(conn)
        got, want = whole_index(conn), fresh(binary, root)
        print(f"random operations: {steps}, live index {len(got)} entries, fresh index {len(want)}, "
              f"missing {len(want - got)}, extra {len(got - want)}")
        if got != want:
            failures.append("random operations")

        limit = int(open("/proc/sys/fs/inotify/max_queued_events").read())
        burst = os.path.join(root, "burst")
        os.makedirs(burst)
        whole_index(conn)
        os.kill(proc.pid, signal.SIGSTOP)
        for i in range(limit + 2000):
            open(os.path.join(burst, f"b{i}"), "w").close()
        os.kill(proc.pid, signal.SIGCONT)
        got, want = whole_index(conn), fresh(binary, root)
        print(f"queue overflow: {limit + 2000} events, identical {got == want}")
        if got != want:
            failures.append("overflow")
    finally:
        proc.terminate()
        proc.wait()
        shutil.rmtree(work, ignore_errors=True)
    print("FAILED: " + ", ".join(failures) if failures else "all checks passed")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
