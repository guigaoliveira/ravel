#!/usr/bin/env python3
"""Measure real stdio MCP sessions on independently indexed copies of one corpus.

Linux, Python standard library only. Never point --work at a source checkout.
Example: mcp_merge_bench.py --corpus CORPUS --work SCRATCH --output result.json
         --binary before=/abs/ravel-1.19 --binary merged=/abs/ravel-merged
The JSON retains every timing sample, CPU counters, response hashes, and RSS samples.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import statistics
import subprocess
import threading
import time


TOOLS = [
    ("status", "status", {}),
    ("exact", "explore", {"query": "helper0_10", "limit": 10}),
    ("ambiguous", "explore", {"query": "method0", "limit": 5}),
    ("terms", "explore", {"query": "svc helper value", "limit": 5}),
    ("callers", "callers_of", {"node": "helper0_0", "limit": 20}),
    ("callees", "calls_from", {"node": "Svc3_100", "limit": 20}),
]
COLD = [
    ["status"],
    ["context", "helper0_10", "--limit", "10"],
    ["context", "method0", "--limit", "5"],
    ["callers-of", "helper0_0", "--limit", "20"],
    ["hubs", "--limit", "20"],
]


def digest(data):
    return hashlib.sha256(data).hexdigest()


def normalized(value, root):
    if isinstance(value, dict):
        return {k: normalized(v, root) for k, v in value.items()
                if k not in ("sid", "snapshot_id", "auto_synced")}
    if isinstance(value, list):
        return [normalized(v, root) for v in value]
    if isinstance(value, str):
        return value.replace(str(root), "<ROOT>")
    return value


def run_cli(binary, root, args, env):
    start = time.perf_counter_ns()
    with subprocess.Popen([str(binary), "--root", str(root), *args], env=env,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE) as p:
        # These commands emit bounded JSON. Drain both pipes before waiting for rusage.
        stderr = []
        reader = threading.Thread(target=lambda: stderr.append(p.stderr.read()), daemon=True)
        reader.start()
        output = p.stdout.read()
        _, status, usage = os.wait4(p.pid, 0)
        p.returncode = os.waitstatus_to_exitcode(status)
        reader.join()
        if p.returncode:
            raise RuntimeError(f"{args}: {b''.join(stderr).decode()}")
    return {
        "wall_ms": (time.perf_counter_ns() - start) / 1e6,
        "cpu_ms": (usage.ru_utime + usage.ru_stime) * 1000,
        "peak_rss_mib": usage.ru_maxrss / 1024,
        "answer": normalized(json.loads(output), root),
    }


def read_process(pid):
    try:
        stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        status = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/status").read_text().splitlines())
        return {
            "pid": pid,
            # utime, stime and waited-for children's utime/stime. Includes git CPU.
            "cpu_ticks": sum(int(stat[i]) for i in (11, 12, 13, 14)),
            "rss_mib": int(status.get("VmRSS", "0 kB").split()[0]) / 1024,
            "anonymous_mib": int(status.get("RssAnon", "0 kB").split()[0]) / 1024,
            "hwm_mib": int(status.get("VmHWM", "0 kB").split()[0]) / 1024,
        }
    except (OSError, ValueError, IndexError):
        return None


def process_ids(binary, root):
    """Exact executable AND exact root argument, excluding unrelated Ravel sessions."""
    result = []
    for path in Path("/proc").glob("[0-9]*"):
        try:
            args = (path / "cmdline").read_bytes().split(b"\0")
            if os.fsencode(str(root)) in args and (path / "exe").resolve() == binary:
                result.append(int(path.name))
        except OSError:
            pass
    return sorted(result)


def usage(pids, pss=False):
    procs = [p for pid in pids if (p := read_process(pid))]
    result = {
        "cpu_ms": sum(p["cpu_ticks"] for p in procs) * 1000 / os.sysconf("SC_CLK_TCK"),
        "rss_mib": sum(p["rss_mib"] for p in procs),
        "anonymous_mib": sum(p["anonymous_mib"] for p in procs),
        "processes": procs,
    }
    if pss:
        total = 0
        for pid in pids:
            try:
                for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines():
                    if line.startswith("Pss:"):
                        total += int(line.split()[1])
            except OSError:
                pass
        result["pss_mib"] = total / 1024
    return result


class Session:
    def __init__(self, binary, root, env, stderr):
        self.binary, self.root, self.env = binary, root, env
        self.log = open(stderr, "wb")  # Outside the watched corpus.
        self.child = subprocess.Popen([str(binary), "--root", str(root), "mcp"], env=env,
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log)
        self.lines = queue.Queue()
        self.next_id = 0
        self.reader = threading.Thread(target=self.read_lines, daemon=True)
        self.reader.start()
        self.initialize = self.request("initialize", {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "ravel-merge-benchmark", "version": "1"},
        })
        self.request("notifications/initialized", notify=True)
        self.tools = self.request("tools/list")["result"]["tools"]

    def read_lines(self):
        for line in self.child.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def request(self, method, params=None, notify=False):
        self.next_id += 1
        request = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            request["params"] = params
        if not notify:
            request["id"] = self.next_id
        self.child.stdin.write(json.dumps(request).encode() + b"\n")
        self.child.stdin.flush()
        if notify:
            return None
        while True:
            line = self.lines.get(timeout=180)
            if line is None:
                raise RuntimeError("MCP closed before answering")
            response = json.loads(line)
            if response.get("id") == self.next_id:
                if "error" in response:
                    raise RuntimeError(response)
                return response

    def call(self, name, arguments, allow_error=False):
        start = time.perf_counter_ns()
        response = self.request("tools/call", {"name": name, "arguments": arguments})
        elapsed = (time.perf_counter_ns() - start) / 1e6
        result = response["result"]
        if result.get("isError") and not allow_error:
            raise RuntimeError(result)
        text = result["content"][0]["text"]
        try:
            answer = json.loads(text)
        except ValueError:
            if not allow_error:
                raise
            answer = {"error_text": text}
        return elapsed, answer, result.get("isError", False), len(text.encode())

    def close(self):
        self.child.stdin.close()
        try:
            self.child.wait(timeout=20)
        except subprocess.TimeoutExpired:
            self.child.kill()
            self.child.wait()
        subprocess.run([str(self.binary), "--root", str(self.root), "daemon", "stop"],
                       env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
        self.log.close()
        deadline = time.monotonic() + 5
        while process_ids(self.binary, self.root) and time.monotonic() < deadline:
            time.sleep(0.05)
        if process_ids(self.binary, self.root):
            raise RuntimeError("Benchmark daemon did not exit")


def measured_batch(session, name, args, calls, think_ms=0):
    pids = process_ids(session.binary, session.root)
    if len(pids) != 2:
        raise RuntimeError(f"Expected stdio server + one daemon, got {pids}")
    samples = []
    done = threading.Event()

    def sample():
        while not done.is_set():
            samples.append(usage(pids))
            done.wait(0.02)

    monitor = threading.Thread(target=sample, daemon=True)
    before = usage(pids, pss=True)
    monitor.start()
    latencies = []
    hashes = set()
    start = time.perf_counter_ns()
    try:
        for _ in range(calls):
            if think_ms:
                time.sleep(think_ms / 1000)
            elapsed, answer, _, size = session.call(name, args)
            latencies.append(elapsed)
            hashes.add(digest(json.dumps(normalized(answer, session.root), sort_keys=True).encode()))
    finally:
        done.set()
        monitor.join()
    after = usage(pids, pss=True)
    return {
        "calls": calls, "think_ms": think_ms, "latency_ms": latencies,
        "batch_wall_ms": (time.perf_counter_ns() - start) / 1e6,
        "cpu_ms_per_call": (after["cpu_ms"] - before["cpu_ms"]) / calls,
        "memory_before": before, "memory_after": after,
        "sampled_peak_server_rss_mib": max([before["rss_mib"], after["rss_mib"]] +
                                           [s["rss_mib"] for s in samples]),
        "memory_samples": samples, "answer_hashes": sorted(hashes), "response_bytes": size,
    }


def exercise(session):
    """Verify edit -> explicit sync -> query -> restore on the actual MCP path."""
    path = session.root / "packages/p3/src/f100.ts"
    original = path.read_bytes()
    rows = []
    try:
        for step, data in [
            ("noop", original),
            ("body", original + b"\n// benchmark content edit\n"),
            ("structural", original + b"\nexport function mergeReviewAdded() { return 42; }\n"),
            ("restore", original),
        ]:
            path.write_bytes(data)
            before = usage(process_ids(session.binary, session.root), pss=True)
            elapsed, answer, _, size = session.call("sync", {"paths": [str(path.relative_to(session.root))]})
            query_ms, query, _, _ = session.call("explore", {"query": "mergeReviewAdded"})
            found = query.get("detail", {}) or {}
            assert (found.get("name") == "mergeReviewAdded") == (step == "structural"), query
            time.sleep(0.2)  # Publication's memory release runs after its reply.
            after = usage(process_ids(session.binary, session.root), pss=True)
            rows.append({"step": step, "wall_ms": elapsed, "query_ms": query_ms,
                         "cpu_ms_including_verification": after["cpu_ms"] - before["cpu_ms"],
                         "memory_before": before, "memory_after": after, "response_bytes": size,
                         "answer": normalized(answer, session.root), "verified": True})
    finally:
        path.write_bytes(original)
    _, first, _, _ = session.call("callers_of", {"node": "helper0_0", "limit": 1})
    cursor = first.get("next_cursor")
    paging = {"first_total": first.get("total"), "cursor": cursor}
    if cursor is not None:
        try:
            _, page, error, _ = session.call("callers_of", {"node": "helper0_0", "limit": 1,
                                                           "cursor": cursor}, allow_error=True)
            paging.update({"second_page_error": error, "second_page": normalized(page, session.root)})
        except RuntimeError as error:
            paging.update({"second_page_error": True, "protocol_error": str(error)})
    return {"edits": rows, "paging": paging}


def assert_equivalent(rows, pairs):
    """Compare behavior separately from timings and snapshot/disk identities."""
    for pair in pairs:
        left, right = pair.split("=", 1)
        a, b = rows[left], rows[right]
        assert a["tools"] == b["tools"], (pair, "tool schemas")
        for one, two in zip(a["initial"], b["initial"], strict=True):
            assert one["step"] == two["step"]
            if one["step"] == "status":
                for field in ("coverage", "stats", "schema", "extensions", "diagnostics"):
                    assert one["answer"][field] == two["answer"][field], (pair, field)
            else:
                assert one["answer"] == two["answer"], (pair, one["step"])
        for step in ("exact", "ambiguous", "terms", "callers", "callees"):
            assert a["batches"][step]["answer_hashes"] == b["batches"][step]["answer_hashes"], (pair, step)
        if "functional" in a:
            assert a["functional"]["paging"] == b["functional"]["paging"], (pair, "paging")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", action="append", required=True, help="label=/absolute/path")
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--calls", type=int, default=100)
    parser.add_argument("--cold-runs", type=int, default=7)
    parser.add_argument("--paced-calls", type=int, default=20)
    parser.add_argument("--skip-cold", action="store_true")
    parser.add_argument("--skip-edits", action="store_true")
    parser.add_argument("--explore-first", action="store_true", help="Warm up with explore before status")
    parser.add_argument("--assert-equivalent", action="append", default=[], help="Compare labels A=B")
    args = parser.parse_args()
    bins = {label: Path(binary).resolve() for label, binary in (v.split("=", 1) for v in args.binary)}
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    for key in list(env):
        if key.startswith("RAVEL_"):
            env.pop(key)
    # Daemon sockets and stderr are outside the watched tree and isolated from user sessions.
    runtime = work / "runtime"
    runtime.mkdir(mode=0o700, exist_ok=True)
    env["XDG_RUNTIME_DIR"] = str(runtime)
    roots = {}
    result = {"binaries": {}, "index": {}, "cold": [], "sessions": [],
              "clock_ticks_per_second": os.sysconf("SC_CLK_TCK"),
              "cpu_affinity": sorted(os.sched_getaffinity(0)), "options": vars(args).copy()}
    result["options"] = {k: str(v) if isinstance(v, Path) else v for k, v in result["options"].items()}
    for label, binary in bins.items():
        result["binaries"][label] = {"sha256": digest(binary.read_bytes()),
                                      "version": subprocess.check_output([str(binary), "--version"]).decode().strip()}
        root = work / label
        if root.exists():
            raise RuntimeError(f"Refusing to overwrite {root}; use a fresh --work")
        shutil.copytree(args.corpus, root, ignore=shutil.ignore_patterns(".ravel"))
        roots[label] = root
        result["index"][label] = run_cli(binary, root, ["index"], env)
        print(json.dumps({"index": label, **{k: v for k, v in result["index"][label].items() if k != "answer"}}), flush=True)
    if not args.skip_cold:
        for command in COLD:
            for run in range(args.cold_runs):
                labels = list(bins) if run % 2 == 0 else list(reversed(bins))
                for label in labels:
                    row = run_cli(bins[label], roots[label], command, env)
                    result["cold"].append({"label": label, "run": run, "command": command, **row})
            print(json.dumps({"cold_complete": command}), flush=True)
    for round_id in range(args.rounds):
        sessions = {}
        rows = {}
        order = list(bins) if round_id % 2 == 0 else list(reversed(bins))
        try:
            for label in order:
                start = time.perf_counter_ns()
                session = Session(bins[label], roots[label], env, work / f"{label}-{round_id}.stderr")
                sessions[label] = session
                rows[label] = {"label": label, "round": round_id,
                               "initialize_ms": (time.perf_counter_ns() - start) / 1e6,
                               "tools": session.tools, "batches": {}}
                initial = []
                initial_tools = TOOLS[1:2] + TOOLS[:1] + TOOLS[2:] if args.explore_first else TOOLS
                for step, name, tool_args in initial_tools:
                    pids = process_ids(bins[label], roots[label])
                    before = usage(pids)
                    elapsed, answer, error, size = session.call(name, tool_args)
                    after = usage(pids, pss=True)
                    initial.append({"step": step, "wall_ms": elapsed, "error": error, "response_bytes": size,
                                    "answer": normalized(answer, roots[label]),
                                    "cpu_ms": after["cpu_ms"] - before["cpu_ms"],
                                    "memory_after": after})
                rows[label]["initial"] = initial
                rows[label]["resident_after_warmup"] = usage(process_ids(bins[label], roots[label]), pss=True)
            for tool_index, (step, name, tool_args) in enumerate(TOOLS):
                for label in (order if tool_index % 2 == 0 else list(reversed(order))):
                    rows[label]["batches"][step] = measured_batch(sessions[label], name, tool_args, args.calls)
                    batch = rows[label]["batches"][step]
                    print(json.dumps({"round": round_id, "label": label, "step": step,
                                      "p50_ms": statistics.median(batch["latency_ms"]),
                                      "cpu_ms_per_call": batch["cpu_ms_per_call"],
                                      "rss_mib": batch["memory_after"]["rss_mib"]}), flush=True)
            if args.paced_calls:
                for label in reversed(order):
                    rows[label]["paced_exact"] = measured_batch(sessions[label], "explore",
                                                                 {"query": "helper0_10", "limit": 10},
                                                                 args.paced_calls, think_ms=75)
            for label in order:
                if not args.skip_edits:
                    rows[label]["functional"] = exercise(sessions[label])
                result["sessions"].append(rows[label])
        finally:
            for session in sessions.values():
                session.close()
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + "\n")
        assert_equivalent(rows, args.assert_equivalent)
    print(json.dumps({"complete": str(args.output), "sessions": len(result["sessions"])}), flush=True)


if __name__ == "__main__":
    main()
