#!/usr/bin/env python3
"""Result parity between two lince binaries on generated requests.

Usage: parity.py ROOT BASELINE CANDIDATE [--kind any|slash|exclude|all]
                 [--requests 300] [--seed 1]

Both daemons load copies of the same snapshot, built by BASELINE, so entry
ids and order agree. Every response must match exactly: result arrays,
their order and both match counts. Requests are generated from the names
and paths under ROOT:
  any      terms from real names (substrings, words, initials, misses),
           1-3 terms, with kinds, extensions, scopes, depth, select first,
           small caps and the occasional exclude;
  slash    "/" terms across components, with leading, trailing and double
           slashes, case and accents;
  exclude  extension, anchored, directory-only, "**", negated and combined
           exclude patterns.
`tests/bench_compare.py` checks parity on its fixed 65 cases; this covers
the request space more widely. Exits non-zero on any mismatch.
"""
import argparse
import os
import random
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench_compare import Daemon, identity  # noqa: E402


def rel_paths(root, limit=200_000):
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        rel = os.path.relpath(dirpath, root)
        for name in dirnames + filenames:
            out.append(name if rel == "." else f"{rel}/{name}")
            if len(out) >= limit:
                return out
    return out


def any_requests(paths, rng, n):
    names = [p.rsplit("/", 1)[-1] for p in paths]
    dirs = sorted({p.rsplit("/", 1)[0] for p in paths if "/" in p})

    def term():
        r = rng.random()
        name = rng.choice(names)
        if r < 0.35 and len(name) > 2:
            a = rng.randrange(len(name) - 1)
            return name[a:a + rng.randint(2, 8)].strip() or "a"
        if r < 0.55:
            return rng.choice(name.replace("_", " ").replace("-", " ").replace(".", " ").split() or ["a"])
        if r < 0.65:
            return rng.choice(["xyzzyq", "qqqq", "zzz9"])
        if r < 0.75:
            p = rng.choice(paths)
            i = p.find("/")
            return p[max(0, i - 3):i + 4] if i > 0 else name[:4]
        if r < 0.85:
            # Every other letter: exercises the fuzzy pass.
            return "".join(c for c in name if c.isalpha())[:6:2] or "omf"
        return rng.choice(["a", "e", "s", "co", "config", "tema", "informe", "pdf"])

    reqs = []
    for _ in range(n):
        req = {"query": " ".join(term() for _ in range(rng.choice([1, 1, 1, 2, 3]))),
               "hidden": rng.random() < 0.6, "max_results": rng.choice([2000, 2000, 60, 5, 1])}
        r = rng.random()
        if r < 0.25:
            req["kinds"] = ["d"]
        elif r < 0.5:
            req["kinds"] = ["f"]
        if rng.random() < 0.2:
            req["extensions"] = rng.sample(["pdf", "md", "txt", "rs", "png", "tar.gz", "json"], 2)
        if rng.random() < 0.15 and dirs:
            req["scopes"] = [rng.choice(dirs)]
        if rng.random() < 0.15:
            req["max_depth"] = rng.randint(1, 4)
        if rng.random() < 0.15:
            req["select"] = "first"
        if rng.random() < 0.1:
            req["exclude"] = [rng.choice(["*.json", "*.pdf", "Projects", "/Documents", "tmp/"])]
        reqs.append(req)
    return reqs


def slash_requests(paths, rng, n):
    terms = ["/", "//", "a/", "/a", "a//b", "/.", "./", "-/", "/-", "x/y/z", "projects/", "/config",
             "Música/", "musica/tema", "CONFIG/", ".config/hypr", "s/", "/s", "a/b"]
    while len(terms) < n:
        p = rng.choice(paths)
        slashes = [i for i, c in enumerate(p) if c == "/"]
        if not slashes:
            continue
        s = rng.choice(slashes)
        t = p[rng.randint(max(0, s - 12), s):rng.randint(s + 1, min(len(p), s + 14))]
        if rng.random() < 0.2:
            t = t.upper()
        if " " not in t and t.strip():
            terms.append(t)
    filters = [{}, {"kinds": ["d"]}, {"kinds": ["f"]}, {"kinds": ["f"], "extensions": ["pdf", "md"]}]
    reqs = []
    for t in terms[:n]:
        query = t if rng.random() < 0.7 else f"{t} {rng.choice(['a', 'config', 'e', 'tema'])}"
        reqs.append({"query": query, "hidden": rng.random() < 0.7,
                     "max_results": rng.choice([2000, 60, 5]), **rng.choice(filters)})
    return reqs


def exclude_requests(paths, rng, n):
    comps = sorted({c for p in paths[:50_000] for c in p.split("/")})
    exts = ["json", "pdf", "md", "txt", "png", "rs", "py", "mp3", "JSON"]

    def pattern():
        r = rng.random()
        c = rng.choice(comps)
        if r < 0.25:
            return f"*.{rng.choice(exts)}"
        if r < 0.40:
            return c
        if r < 0.50:
            return f"/{c}"
        if r < 0.60:
            return f"{c}/"
        if r < 0.70:
            return f"{c[:3]}*"
        if r < 0.80:
            return f"**/{c}/*.{rng.choice(exts)}"
        if r < 0.88:
            return "/".join(rng.choice(paths).split("/")[:2])
        if r < 0.94:
            return f"!*.{rng.choice(exts)}"
        return f"*{c[-2:]}"

    filters = [{}, {"kinds": ["d"]}, {"kinds": ["f"]}]
    return [{"query": rng.choice(["config", "a", "", "tema", "e", "informe", "readme", "s"]),
             "exclude": [pattern() for _ in range(rng.choice([1, 1, 2, 3]))],
             "hidden": rng.random() < 0.7, "max_results": rng.choice([2000, 60, 5]), **rng.choice(filters)}
            for _ in range(n)]


GENERATORS = {"any": any_requests, "slash": slash_requests, "exclude": exclude_requests}


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for name in ["root", "baseline", "candidate"]:
        p.add_argument(name)
    p.add_argument("--kind", choices=[*GENERATORS, "all"], default="all")
    p.add_argument("--requests", type=int, default=300, help="per kind")
    p.add_argument("--seed", type=int, default=1)
    a = p.parse_args()
    root = str(Path(a.root).resolve())
    paths = rel_paths(root)
    if not paths:
        p.error(f"{root} is empty")
    rng = random.Random(a.seed)
    kinds = list(GENERATORS) if a.kind == "all" else [a.kind]
    batches = [(k, GENERATORS[k](paths, rng, a.requests)) for k in kinds]
    failed = 0
    with tempfile.TemporaryDirectory(prefix="lince-parity-") as tmp:
        work = Path(tmp)
        seed = Daemon(a.baseline, root, work, "seed")
        seed.close()  # persist the exact ids/order both engines will use
        daemons = []
        try:
            for name in ["baseline", "candidate"]:
                daemons.append(Daemon(getattr(a, name), root, work, name, seed.snapshot))
            for kind, reqs in batches:
                bad = matched = 0
                for req in reqs:
                    base, cand = (d.ask(req)[1] for d in daemons)
                    if identity(base) != identity(cand):
                        bad += 1
                        if bad <= 5:
                            print(f"MISMATCH {req}: {base['matched_dirs']}+{base['matched_files']} vs "
                                  f"{cand['matched_dirs']}+{cand['matched_files']}", flush=True)
                    matched += bool(base["matched_dirs"] + base["matched_files"])
                print(f"{kind:8} {len(reqs) - bad}/{len(reqs)} identical ({matched} with matches)", flush=True)
                failed += bad
        finally:
            for d in daemons:
                d.close()
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
