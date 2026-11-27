#!/usr/bin/env python3
"""Verify that a slow Unix-socket reader does not delay other clients."""

import json
import select
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def connect(path):
    conn = socket.socket(socket.AF_UNIX)
    conn.settimeout(5)
    conn.connect(str(path))
    return conn


def check(binary, work):
    root = work / "root"
    root.mkdir()
    count = 6_000
    for i in range(count):
        (root / f"{i:04d}-{'a' * 200}.txt").touch()

    path = work / "lince.sock"
    config = work / "config.json"
    config.write_text("{}")
    proc = subprocess.Popen([
        binary, "serve", "--root", root, "--socket", path,
        "--snapshot", work / "index.lnc", "--config", config,
    ])
    browse = json.dumps({"id": 3, "kinds": ["f"], "max_results": count}).encode() + b"\n"
    try:
        until = time.monotonic() + 10
        while not path.exists():
            if proc.poll() is not None or time.monotonic() >= until:
                raise RuntimeError("daemon did not start")
            time.sleep(.01)

        with connect(path) as slow:
            slow.sendall(browse)
            assert slow.recv(1, socket.MSG_PEEK), "large reply never started"
            with connect(path) as fast:
                fast.settimeout(.75)
                fast.sendall(b'{"op":"status"}\n')
                with fast.makefile("rb") as reply:
                    assert json.loads(reply.readline())["entries"] == count
                    new_file = root / "just-created.txt"
                    new_file.touch()
                    fast.sendall(b'{"query":"just-created"}\n')
                    result = json.loads(reply.readline())
                    assert result["files"] == [str(new_file)]
                    assert result["fresh"]
                    new_file.unlink()

            poll = select.poll()
            poll.register(slow, select.POLLHUP)
            assert poll.poll(5_000), "daemon kept a stalled client open"

        # Partial writes preserve complete lines, ordered replies, and clients
        # that close their write side before reading.
        with connect(path) as conn:
            conn.sendall(b'{"id":1}\n{"id":2}\n' + browse)
            conn.shutdown(socket.SHUT_WR)
            with conn.makefile("rb") as reply:
                for request_id in (1, 2):
                    assert json.loads(reply.readline()) == {"v": 1, "id": request_id, "superseded": True}
                result = json.loads(reply.readline())
                assert result["id"] == 3
                assert len(result["files"]) == count
                assert reply.read() == b""
    finally:
        proc.terminate()
        proc.wait(timeout=10)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="lince-socket-test-") as run:
        check(Path(sys.argv[1]).resolve(), Path(run))
    print("Socket backpressure checks passed")
