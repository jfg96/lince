#!/usr/bin/env python3
"""Paired search benchmark and result parity on a frozen tree.

Usage: bench_compare.py ROOT BASELINE CANDIDATE OUT.json [--repeats 30]
Both daemons load copies of the same snapshot. Do not change ROOT during the
run. Watches are disabled to isolate query cost; live updates have their own
stress test. Timings include engine phases, socket/JSON wall time and CPU.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import statistics
import subprocess
import tempfile
import time


QUERIES = ["hypr", "omarchy find", "cancion", "informe", "config", "a", "pdf",
           "projects/omarchy", "omfind", "xyzzyq", "bases datos", "main rs", ""]
EXCLUDES = [".git", "node_modules", ".cache", "__pycache__", ".venv", "venv"]


def cases():
    out = []
    filters = [{}, {"kinds": ["d"]}, {"kinds": ["f"], "extensions": ["pdf", "md", "txt"]},
               {"kinds": ["f"], "extensions": ["rs", "py", "js", "cpp", "qml"]}]
    for query in QUERIES:
        for f in filters:
            out.append({"query": query, "hidden": True, "max_results": 2000, **f})
    # Small caps stress boundary ties; first must preserve name/path pass order.
    for query in ["a", "config", ""]:
        for cap in [1, 60]:
            out.append({"query": query, "hidden": True, "max_results": cap})
        out.append({"query": query, "hidden": True, "max_results": 60, "select": "first"})
    out += [{"query": "a", "scopes": ["Projects"], "max_depth": 3},
            {"query": "config", "exclude": ["*.json"]},
            {"query": "a a", "max_results": 60},
            {"query": "canción", "hidden": False, "max_results": 60}]
    return out


class Daemon:
    def __init__(self, binary, root, work, name, seed=None):
        self.snapshot = work / (name + ".lnc")
        if seed:
            shutil.copyfile(seed, self.snapshot)
        sock = work / (name + ".sock")
        cfg = work / "config.json"
        cfg.write_text("{}")
        argv = [str(Path(binary).resolve()), "serve", "--config", str(cfg), "--root", root,
                "--socket", str(sock), "--snapshot", str(self.snapshot), "--no-watch"]
        for e in EXCLUDES:
            argv += ["--exclude", e]
        self.log = open(work / (name + ".log"), "w+")
        self.proc = subprocess.Popen(argv, stderr=self.log)
        try:
            deadline = time.monotonic() + 120
            while True:
                self.sock = socket.socket(socket.AF_UNIX)
                self.sock.settimeout(30)
                try:
                    self.sock.connect(str(sock))
                    break
                except (FileNotFoundError, ConnectionRefusedError):
                    self.sock.close()
                    if self.proc.poll() is not None or time.monotonic() > deadline:
                        self.log.seek(0)
                        raise RuntimeError(self.log.read())
                    time.sleep(.005)
            self.file = self.sock.makefile("rb")
        except BaseException:
            self.close()
            raise

    def ask(self, req):
        start = time.perf_counter_ns()
        self.sock.sendall((json.dumps(req) + "\n").encode())
        r = json.loads(self.file.readline())
        if "error" in r:
            raise RuntimeError(r)
        return (time.perf_counter_ns() - start) / 1e6, r

    def cpu(self):
        f = Path(f"/proc/{self.proc.pid}/stat").read_text().rsplit(")", 1)[1].split()
        return (int(f[11]) + int(f[12])) * 1000 / os.sysconf("SC_CLK_TCK")

    def close(self):
        if hasattr(self, "file"):
            self.file.close()
        if hasattr(self, "sock"):
            self.sock.close()
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()


def identity(r):
    return [r["dirs"], r["files"], r["matched_dirs"], r["matched_files"]]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ["root", "baseline", "candidate", "output"]:
        p.add_argument(name)
    p.add_argument("--repeats", type=int, default=30)
    p.add_argument("--query", action="append", help="only cases with this query (repeatable)")
    a = p.parse_args()
    if a.repeats < 1:
        p.error("--repeats must be positive")
    rows = []
    with tempfile.TemporaryDirectory(prefix="lince-paired-") as tmp:
        work = Path(tmp)
        seed = Daemon(a.baseline, str(Path(a.root).resolve()), work, "seed")
        seed.close()  # persist the exact ids/order both engines will use
        daemons = []
        try:
            for name in ["baseline", "candidate"]:
                daemons.append(Daemon(getattr(a, name), str(Path(a.root).resolve()), work, name, seed.snapshot))
            for req in cases():
                if a.query is not None and req["query"] not in a.query:
                    continue
                first = [d.ask(req)[1] for d in daemons]
                if identity(first[0]) != identity(first[1]):
                    raise AssertionError(f"result/order/count mismatch: {req}")
                for d in daemons:
                    for _ in range(3):
                        d.ask(req)
                samples = [[], []]
                cpu = [0., 0.]
                # Alternate short batches to limit drift and tick quantisation.
                for batch in range(6):
                    for i in ([0, 1] if batch % 2 == 0 else [1, 0]):
                        d = daemons[i]
                        before = d.cpu()
                        for _ in range(a.repeats):
                            wall, r = d.ask(req)
                            samples[i].append([wall, r["engine_us"] / 1000, *r["phases_us"]])
                            if identity(r) != identity(first[0]):
                                raise AssertionError(f"unstable results: {req}")
                        cpu[i] += d.cpu() - before
                row = {"request": req, "matches": first[0]["matched_dirs"] + first[0]["matched_files"]}
                for i, name in enumerate(["baseline", "candidate"]):
                    row[name] = {"wall_ms": statistics.median(s[0] for s in samples[i]),
                                 "engine_ms": statistics.median(s[1] for s in samples[i]),
                                 "phases_us": [statistics.median(s[j] for s in samples[i]) for j in range(2, 6)],
                                 "cpu_ms": cpu[i] / len(samples[i]),
                                 "samples": samples[i]}
                rows.append(row)
                print(f"{req['query']!r:22} {row['matches']:8} "
                      f"{row['baseline']['engine_ms']:7.3f} -> {row['candidate']['engine_ms']:7.3f} ms", flush=True)
            statuses = [d.ask({"op": "status"})[1] for d in daemons]
        finally:
            for d in daemons:
                d.close()
    result = {"root": str(Path(a.root).resolve()), "repeats_per_case": a.repeats * 6,
              "platform": {"system": os.uname().sysname, "release": os.uname().release, "machine": os.uname().machine, "cpu_count": os.cpu_count(),
                           "LINCE_THREADS": os.environ.get("LINCE_THREADS", "default")},
              "binary_sha256": {n: hashlib.sha256(Path(getattr(a, n)).read_bytes()).hexdigest()
                                for n in ["baseline", "candidate"]},
              "status": statuses, "results": rows}
    Path(a.output).write_text(json.dumps(result, indent=2) + "\n")
    for name in ["baseline", "candidate"]:
        print(name, {k: round(statistics.median(r[name][k] for r in rows), 3)
                     for k in ["wall_ms", "engine_ms", "cpu_ms"]})
    print(f"Exact results, order and counts: {len(rows)}/{len(rows)} cases")


if __name__ == "__main__":
    main()
