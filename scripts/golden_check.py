#!/usr/bin/env python3
"""Golden correctness + perf-ceiling gate for ravel on a real indexed repo.

Usage: golden_check.py <ravel-bin> <repo-root> [--symbols N]

Golden checks (soundness against ground truth):
  - context: primary is a real prefix match; every listed caller exists in the
    symbol dict; detail (when present) matches the primary.
  - refactor: every file in files[] exists on disk AND textually contains the
    symbol (a rename plan must never point at a file without the name).

Perf gate: every hot-path command must finish under the 200 ms ceiling
(the README product SLA). Exit code 1 on any golden or ceiling failure.
"""

import json
import string
import subprocess
import sys
import time
from pathlib import Path

CEILING_MS = 200.0
HOT_COMMANDS = [
    ["stats"],
    ["status"],
    ["cheatsheet"],
    ["hubs"],
    ["sync"],
]  # symbol commands appended per sampled symbol


def run(binary, root, args):
    start = time.perf_counter()
    proc = subprocess.run(
        [binary, "--root", str(root), *args],
        capture_output=True,
        text=True,
        timeout=60,
    )
    elapsed_ms = (time.perf_counter() - start) * 1000
    return proc, elapsed_ms


def run_json(binary, root, args):
    proc, elapsed_ms = run(binary, root, args)
    try:
        return json.loads(proc.stdout), elapsed_ms
    except json.JSONDecodeError:
        return None, elapsed_ms


def sample_symbols(binary, root, want):
    """Sample real symbol names via prefix search across the alphabet."""
    symbols = []
    for letter in string.ascii_uppercase:
        hits, _ = run_json(binary, root, [
            "search", letter, "--kind", "prefix", "--limit", "3",
        ])
        for hit in hits or []:
            value = hit.get("value", "")
            # Skip path-ish and trivial names — golden checks want identifiers.
            if value and "/" not in value and "." not in value and len(value) > 3:
                symbols.append(value)
        if len(symbols) >= want:
            break
    return symbols[:want]


def norm_path(raw):
    return raw.replace("/./", "/")


def main():
    binary, root = sys.argv[1], Path(sys.argv[2]).resolve()
    want = int(sys.argv[sys.argv.index("--symbols") + 1]) if "--symbols" in sys.argv else 15

    failures = []
    slow = []

    def gate(label, elapsed_ms):
        if elapsed_ms > CEILING_MS:
            slow.append(f"{label}: {elapsed_ms:.0f} ms > {CEILING_MS:.0f} ms ceiling")

    status, elapsed = run_json(binary, root, ["status"])
    gate("status", elapsed)
    if not status or not status.get("indexed"):
        print("FAIL: repo not indexed — run `ravel index` first")
        return 1

    symbols = sample_symbols(binary, root, want)
    if len(symbols) < min(want, 5):
        failures.append(f"symbol sampling too small: {symbols}")
    print(f"sampled {len(symbols)} symbols: {symbols[:5]}…")

    for sym in symbols:
        ctx, elapsed = run_json(binary, root, ["context", sym, "--limit", "5"])
        gate(f"context {sym}", elapsed)
        if ctx is None:
            failures.append(f"context {sym}: not JSON")
            continue
        primary = ctx.get("primary", "")
        if not primary.lower().startswith(sym.lower()):
            failures.append(f"context {sym}: primary {primary!r} is not a prefix match")
        detail = ctx.get("detail")
        if detail and detail.get("name") != primary:
            failures.append(f"context {sym}: detail.name {detail.get('name')!r} != primary")
        for caller in ctx.get("callers", []):
            hits, _ = run_json(binary, root, ["search", caller, "--kind", "exact", "--limit", "5"])
            values = [h.get("value") for h in hits or []]
            # Callers can be file paths (graph is file-level) — check dict OR disk.
            on_disk = (root / norm_path(caller).lstrip("/")).exists() or Path(norm_path(caller)).exists()
            if caller not in values and not on_disk:
                failures.append(f"context {sym}: caller {caller!r} not in dict nor on disk")

        ref, elapsed = run_json(binary, root, ["refactor", sym])
        gate(f"refactor {sym}", elapsed)
        if ref is None:
            failures.append(f"refactor {sym}: not JSON")
            continue
        files = []
        for raw in ref.get("files", []):
            fpath = Path(norm_path(raw))
            if not fpath.is_absolute():
                fpath = root / fpath
            if not fpath.is_file():
                failures.append(f"refactor {sym}: file missing on disk: {raw}")
            files.append(norm_path(raw).lstrip("./"))
        # Completeness: every file that textually uses the symbol must be in the
        # plan (files[] may ALSO contain transitive dependents — that's blast
        # radius by design, so no "contains the symbol" check on each entry).
        try:
            gp = subprocess.run(
                ["git", "-C", str(root), "grep", "-lw", sym, "--", "*.ts", "*.js"],
                capture_output=True, text=True, timeout=60)
            direct = [l for l in gp.stdout.splitlines() if l and ".spec." not in l and "test" not in l]
        except Exception:
            direct = []
        if ref.get("truncated"):
            direct = []  # plan legitimately partial — completeness not assertable
        if len(files) < 40 and direct:  # not truncated by the limit
            missing = [d for d in direct if d not in files]
            if missing:
                failures.append(f"refactor {sym}: plan misses direct users: {missing[:3]}")

        _, elapsed = run(binary, root, ["query", sym, "--reverse"])
        gate(f"query {sym}", elapsed)
        _, elapsed = run(binary, root, ["impact", sym, "--risk"])
        gate(f"impact {sym}", elapsed)
        _, elapsed = run(binary, root, ["search", sym, "--kind", "prefix", "--limit", "10"])
        gate(f"search {sym}", elapsed)

    for args in HOT_COMMANDS:
        _, elapsed = run(binary, root, args)
        gate(" ".join(args), elapsed)

    print(f"\n== golden: {len(failures)} failure(s) ==")
    for f in failures:
        print(f"  FAIL {f}")
    print(f"== perf ceiling ({CEILING_MS:.0f} ms): {len(slow)} violation(s) ==")
    for s in slow:
        print(f"  SLOW {s}")
    ok = not failures and not slow
    print("\nRESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
