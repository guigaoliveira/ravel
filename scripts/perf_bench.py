#!/usr/bin/env python3
"""End-to-end CPU/RSS benchmark for one ravel binary on one corpus.

Usage: perf_bench.py <ravel-bin> <corpus-src> <work-dir> <label>
                     --symbol NAME [--index-runs N] [--edit-file REL]

Copies the corpus to <work-dir>/<label>, runs a full index, cold read commands and
three kinds of sync on --edit-file (unchanged, content-only, structural), and prints
one JSON object {step: {wall_ms, cpu_ms, rss_mb}}. Every step reports the child's
own rusage via os.wait4: wall time, user+system CPU, peak RSS (median of N runs).
Run two binaries one after the other on the same corpus to compare them.
"""
import json
import os
import shutil
import statistics
import subprocess
import sys
import time

binary, src, work, label = sys.argv[1:5]
rest = sys.argv[5:]
index_runs = 1
edit_file = None
symbol = None
while rest:
    if rest[0] == "--index-runs":
        index_runs = int(rest[1]); rest = rest[2:]
    elif rest[0] == "--edit-file":
        edit_file = rest[1]; rest = rest[2:]
    elif rest[0] == "--symbol":
        symbol = rest[1]; rest = rest[2:]
    else:
        raise SystemExit(f"bad arg {rest[0]}")
if symbol is None:
    raise SystemExit("--symbol is required")

root = os.path.join(work, label)
if os.path.exists(root):
    shutil.rmtree(root)
shutil.copytree(src, root, symlinks=True)
env = dict(os.environ)
env.pop("RAVEL_TIMING", None)


def run(args, n=1, capture=False):
    walls, cpus, rsss = [], [], []
    out = None
    for _ in range(n):
        start = time.perf_counter()
        proc = subprocess.Popen(
            [binary, "--root", root, *args],
            stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=env,
        )
        data = proc.stdout.read() if capture else None
        _, status, ru = os.wait4(proc.pid, 0)
        if os.waitstatus_to_exitcode(status) != 0:
            raise SystemExit(f"{args} failed rc={os.waitstatus_to_exitcode(status)}")
        walls.append((time.perf_counter() - start) * 1000)
        cpus.append((ru.ru_utime + ru.ru_stime) * 1000)
        rsss.append(ru.ru_maxrss / 1024)
        out = data
    result = {
        "wall_ms": round(statistics.median(walls), 1),
        "cpu_ms": round(statistics.median(cpus), 1),
        "rss_mb": round(statistics.median(rsss), 1),
    }
    return (result, out) if capture else result


results = {}
for i in range(index_runs):
    if i:
        shutil.rmtree(os.path.join(root, ".ravel"), ignore_errors=True)
    r = run(["index"])
    results.setdefault("_index_runs", []).append(r)
results["index"] = {
    k: round(statistics.median(x[k] for x in results["_index_runs"]), 1)
    for k in ("wall_ms", "cpu_ms", "rss_mb")
}
del results["_index_runs"]

results["stats"] = run(["stats"], n=5)
results["status"] = run(["status"], n=5)
results["search"] = run(["search", symbol, "--kind", "prefix", "--limit", "20"], n=5)
results["query_reverse"] = run(["query", symbol, "--reverse"], n=5)
results["impact"] = run(["impact", symbol], n=5)
results["context"] = run(["context", symbol, "--limit", "5"], n=5)
results["hubs"] = run(["hubs"], n=5)

if edit_file:
    path = os.path.join(root, edit_file)
    with open(path) as f:
        original = f.read()
    results["sync_noop"] = run(["sync", edit_file], n=3)
    # content-only edits: alternate a trailing comment so every run changes bytes
    samples = []
    for k in range(3):
        with open(path, "w") as f:
            f.write(original + f"\n// edit {k}\n")
        samples.append(run(["sync", edit_file]))
    results["sync_content"] = {
        key: round(statistics.median(s[key] for s in samples), 1)
        for key in ("wall_ms", "cpu_ms", "rss_mb")
    }
    samples = []
    for k in range(3):
        with open(path, "w") as f:
            f.write(original + f"\nexport function addedFn{k}(): number {{ return {k}; }}\n")
        samples.append(run(["sync", edit_file]))
    results["sync_structural"] = {
        key: round(statistics.median(s[key] for s in samples), 1)
        for key in ("wall_ms", "cpu_ms", "rss_mb")
    }
    with open(path, "w") as f:
        f.write(original)
    results["sync_restore"] = run(["sync", edit_file])

disk = 0
for dirpath, _, names in os.walk(os.path.join(root, ".ravel")):
    for n in names:
        try:
            disk += os.path.getsize(os.path.join(dirpath, n))
        except OSError:
            pass
results["index_disk_mb"] = round(disk / 1048576, 1)
print(json.dumps({"label": label, "results": results}))
