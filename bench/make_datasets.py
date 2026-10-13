#!/usr/bin/env python3
"""Build frozen benchmark trees for comparing search engines.

mirror   Recreates the names of a real home (empty files, same directories,
         symlinks kept as dangling links) so both engines can be measured on
         realistic names without the tree changing under the benchmark.
synth N  Deterministic synthetic home with ~N entries: Spanish/English words,
         accents, hidden config trees, excluded vendor dirs and every category
         extension the UI filters on.
"""

from __future__ import annotations

import os
import random
import subprocess
import sys

BASE_EXCLUDES = [
    ".git", "node_modules", ".cache", "__pycache__", ".npm", ".nvm", ".cargo",
    ".rustup", ".gradle", ".local/share/Trash", ".local/share/flatpak", ".venv", "venv",
]


def mirror(source: str, dest: str) -> None:
    argv = ["fd", "-0", "--no-ignore", "--hidden", "--color=never"]
    for pattern in BASE_EXCLUDES:
        argv += ["-E", pattern]
    count = 0
    for kind in ("d", "f", "l"):
        out = subprocess.run(argv + ["--type", kind, ".", source], capture_output=True, check=True).stdout
        for raw in out.split(b"\0"):
            if not raw:
                continue
            rel = os.path.relpath(os.fsdecode(raw), source)
            target = os.path.join(dest, rel)
            if kind == "d":
                os.makedirs(target, exist_ok=True)
            else:
                os.makedirs(os.path.dirname(target), exist_ok=True)
                if kind == "f":
                    open(target, "ab").close()
                elif not os.path.lexists(target):
                    os.symlink("/nonexistent/ofind-bench", target)
            count += 1
    # Excluded directories exist (so the walker has to prune them) but stay small.
    for name in ("node_modules", ".git", ".cache", "venv"):
        for i in range(20):
            os.makedirs(os.path.join(dest, "Projects", f"vendored{i}", name, "pkg"), exist_ok=True)
            open(os.path.join(dest, "Projects", f"vendored{i}", name, "pkg", "index.js"), "ab").close()
    print(f"mirror: {count} entries -> {dest}")


WORDS = (
    "informe proyecto notas canción música presentación factura contrato examen tema "
    "resumen diseño config backup photo holiday report budget invoice draft final "
    "omarchy hyprland waybar theme kernel shader render player server client test "
    "alcachofa españa camión lección acción niño año datos señal pequeño "
    "readme changelog license main index app utils helpers module widget panel "
    "screenshot wallpaper vacation family travel recipe tutorial lecture slides"
).split()
EXTS = ["pdf", "docx", "md", "txt", "png", "jpg", "mp4", "mkv", "mp3", "flac", "py", "rs",
        "js", "qml", "json", "toml", "zip", "csv", "", "", "sh", "cpp", "svg", "odt"]
TOP = ["Documents", "Downloads", "Pictures", "Music", "Videos", "Projects", "Work", "Música",
       ".config", ".local/share", ".mozilla", ".steam"]


def synth(n: int, dest: str, seed: int = 20261012) -> None:
    rng = random.Random(seed)
    made = 0
    dirs = []
    for top in TOP:
        path = os.path.join(dest, top)
        os.makedirs(path, exist_ok=True)
        dirs.append((path, 1))
        made += 1
    while made < n:
        parent, depth = rng.choice(dirs)
        r = rng.random()
        if r < 0.11 and depth < 9:
            name = "-".join(rng.sample(WORDS, rng.randint(1, 2)))
            if rng.random() < 0.08:
                name = "." + name
            if rng.random() < 0.01:
                name = rng.choice(["node_modules", ".git", "__pycache__", ".venv"])
            path = os.path.join(parent, name)
            if not os.path.exists(path):
                os.mkdir(path)
                dirs.append((path, depth + 1))
                made += 1
        else:
            words = rng.sample(WORDS, rng.randint(1, 3))
            sep = rng.choice(["_", "-", " ", ""])
            name = sep.join(w.capitalize() if rng.random() < 0.3 else w for w in words)
            name += f"{rng.randint(0, 999)}" if rng.random() < 0.5 else ""
            ext = rng.choice(EXTS)
            if ext:
                name += "." + ext
            if rng.random() < 0.03:
                name = "." + name
            path = os.path.join(parent, name)
            if not os.path.exists(path):
                open(path, "ab").close()
                made += 1
    print(f"synth: {made} entries -> {dest}")


def main() -> int:
    if sys.argv[1] == "mirror":
        mirror(sys.argv[2], sys.argv[3])
    elif sys.argv[1] == "synth":
        synth(int(sys.argv[2]), sys.argv[3])
    else:
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
