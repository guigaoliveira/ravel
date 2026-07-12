#!/usr/bin/env python3
"""Property tests against a corpus with KNOWN ground truth (gen_corpus.py layout:
file fN.ts imports fN-1, fN-2, fN-3 in the same package).

1. COMPLETENESS — for sampled files, `query --reverse` must contain every known
   importer (fN+1..fN+3). A missed edge = wrong graph, caught here.
2. EQUIVALENCE — after editing files, `sync` must produce the same snapshot as a
   from-scratch `index` (same stats, same query answers). Proves the incremental
   path never diverges from the full path.

Usage: property_check.py <ravel-bin> <corpus-root>   (exit 1 on any failure)
"""

import json
import shutil
import subprocess
import sys
from pathlib import Path

def run(binary, root, args):
    proc = subprocess.run([binary, "--root", str(root), *args],
                          capture_output=True, text=True, timeout=120)
    return proc.stdout

def run_json(binary, root, args):
    return json.loads(run(binary, root, args))

def main():
    binary, root = sys.argv[1], Path(sys.argv[2]).resolve()
    failures = []

    # --- 1. completeness: known importers must appear in reverse query ---
    for pkg, i in [(3, 50), (10, 100), (20, 7)]:
        node_hits = run_json(binary, root, ["search", f"Service_p{pkg}_f{i}",
                                            "--kind", "exact", "--limit", "1"])
        if not node_hits:
            failures.append(f"completeness: Service_p{pkg}_f{i} not in dict")
            continue
        page = run_json(binary, root, ["query", f"Service_p{pkg}_f{i}",
                                       "--reverse", "--depth", "2"])
        found = " ".join(page.get("items", []))
        # Generator ground truth: f{i+k} imports f{i} for k in 1..3.
        for k in (1, 2, 3):
            expected = f"f{i + k}"
            if expected not in found:
                failures.append(
                    f"completeness: p{pkg}/f{i} reverse query missing importer {expected} "
                    f"(items: {found[:120]}…)")

    # --- 2. equivalence: sync after edits == fresh full index ---
    edited = [root / f"packages/p{p}/src/f{40 + p}.ts" for p in (1, 5, 9)]
    backups = {f: f.read_text() for f in edited}
    try:
        for f in edited:
            f.write_text(backups[f] + "\nexport function propcheck_added(): number { return 1; }\n")
        sync_stats = run_json(binary, root, ["sync"] + [str(f) for f in edited])
        sync_query = run(binary, root, ["context", "propcheck_added", "--limit", "5"])

        shutil.rmtree(root / ".ravel")
        index_stats = run_json(binary, root, ["index"])
        index_query = run(binary, root, ["context", "propcheck_added", "--limit", "5"])

        for key in ("files", "edges", "bytes", "snapshot_id"):
            if sync_stats.get(key) != index_stats.get(key):
                failures.append(f"equivalence: stats.{key} sync={sync_stats.get(key)} "
                                f"index={index_stats.get(key)}")
        # Same question, same answer, regardless of which path built the snapshot.
        s, x = json.loads(sync_query), json.loads(index_query)
        for key in ("primary", "matches", "callers", "callees", "detail"):
            if s.get(key) != x.get(key):
                failures.append(f"equivalence: context.{key} sync={s.get(key)} index={x.get(key)}")
    finally:
        for f, text in backups.items():
            f.write_text(text)
        run(binary, root, ["index"])  # restore clean snapshot

    print(f"== property: {len(failures)} failure(s) ==")
    for f in failures:
        print(f"  FAIL {f}")
    print("RESULT:", "PASS" if not failures else "FAIL")
    return 1 if failures else 0

if __name__ == "__main__":
    sys.exit(main())
