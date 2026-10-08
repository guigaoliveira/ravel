#!/usr/bin/env python3
"""A/B proof for two ravel binaries on one indexed corpus.

Usage: ab_verify.py BASE NEW CORPUS WORK [--reps N] [--mcp-reps N] [--think-ms MS]
                    [--no-callgrind] [--sections cold,mcp,sync,warm]

CORPUS is an indexed tree (scripts/gen_corpus.py, then `ravel index`, ideally git-initialised so
the auto-sync runs as it does for agents); every section works on fresh copies under WORK. Prints
one JSON object per measured step, then `RESULT {...}`.

cold : each agent-frequent CLI command run cold (no daemon). Callgrind instructions over every
       process (git included), median wall/CPU/peak RSS over interleaved runs, and whether stdout
       is byte-identical.
mcp  : one `ravel mcp` session per binary on its own copy, calls interleaved between the two;
       median/p90 latency, CPU of every process of the root (waited-for children included), RSS
       at the end, and whether every tool result matches. --think-ms pauses before each call the
       way an agent does, past the 50 ms dirty-listing cache.
sync : the same edit sequence on a copy per binary (noop, content-only, structural, restore);
       callgrind per step and query equality afterwards.
warm : instructions per call inside a warm daemon and the MCP server, both under callgrind
       (`callgrind_control` zeroes and dumps around each batch; the minimum of five batches).

Instruction counts are the proof on a shared machine: wall time on a loaded host moves by tens of
percent between identical binaries, while instructions stay within a fraction of a percent.
Results are compared after removing the copy's root path and per-generation ids (`sid`).
"""
import glob, json, os, shutil, statistics, subprocess, sys, time

args = sys.argv[1:]
base, new, corpus, work = args[:4]
rest = args[4:]
reps, mcp_reps, callgrind, sections, think_ms = 15, 60, True, {"cold", "mcp", "sync"}, 0
while rest:
    if rest[0] == "--reps": reps = int(rest[1]); rest = rest[2:]
    elif rest[0] == "--mcp-reps": mcp_reps = int(rest[1]); rest = rest[2:]
    elif rest[0] == "--no-callgrind": callgrind = False; rest = rest[1:]
    elif rest[0] == "--sections": sections = set(rest[1].split(",")); rest = rest[2:]
    elif rest[0] == "--think-ms": think_ms = int(rest[1]); rest = rest[2:]
    else: raise SystemExit(f"bad arg {rest[0]}")
os.makedirs(work, exist_ok=True)
env = dict(os.environ); env.pop("RAVEL_TIMING", None)
BINS = {"base": base, "new": new}

COLD = [
    ["context", "Svc3_100", "--limit", "5"],
    ["context", "helper0_10", "--limit", "10"],
    ["context", "method0", "--limit", "5"],
    ["context", "svc helper value", "--limit", "5"],
    ["callers-of", "helper0_0"],
    ["callers-of", "Svc0_0"],
    ["callers-of", "method0"],
    ["callers-of", "helper0_0", "--rollup", "dir"],
    ["calls-from", "Svc3_100"],
    ["status"],
    ["search", "Svc", "--kind", "prefix", "--limit", "20"],
]
MCP = [
    ("status", {}),
    ("explore", {"query": "Svc3_100"}),
    ("explore", {"query": "method0"}),
    ("explore", {"query": "helper0_10", "limit": 20}),
    ("callers_of", {"node": "helper0_0"}),
    ("callers_of", {"node": "Svc0_0", "cursor": 50}),
    ("callers_of", {"node": "method0"}),
    ("callers_of", {"node": "helper0_0", "rollup": "dir"}),
    ("calls_from", {"node": "Svc3_100"}),
]


def copy(label):
    dst = os.path.join(work, label)
    if os.path.exists(dst):
        stop_daemon(BINS.get(label.split("-")[0], base), dst)
        shutil.rmtree(dst)
    shutil.copytree(corpus, dst, symlinks=True)
    return dst


def stop_daemon(binary, root):
    subprocess.run([binary, "--root", root, "daemon", "stop"], capture_output=True, env=env)


def run(binary, root, cmd):
    t = time.perf_counter()
    p = subprocess.Popen([binary, "--root", root, *cmd], stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, env=env)
    out = p.stdout.read()
    _, status, ru = os.wait4(p.pid, 0)
    if os.waitstatus_to_exitcode(status) != 0:
        raise SystemExit(f"{binary} {cmd} rc={os.waitstatus_to_exitcode(status)}")
    return out, (time.perf_counter() - t) * 1000, (ru.ru_utime + ru.ru_stime) * 1000, ru.ru_maxrss / 1024


def instructions(binary, root, cmd, tag):
    if not callgrind:
        return None
    pat = os.path.join(work, f"cg.{tag}.%p")
    for f in glob.glob(os.path.join(work, f"cg.{tag}.*")):
        os.remove(f)
    subprocess.run(["valgrind", "--tool=callgrind", "--trace-children=yes",
                    f"--callgrind-out-file={pat}", binary, "--root", root, *cmd],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
    total = 0
    for f in glob.glob(os.path.join(work, f"cg.{tag}.*")):
        with open(f) as fh:
            for line in fh:
                if line.startswith("summary:") or line.startswith("totals:"):
                    total += int(line.split()[1]); break
        os.remove(f)
    return total


VOLATILE = {"sid", "snapshot_id"}


def norm(data, root):
    """Output with the copy's root path and per-generation ids taken out; everything else compared."""
    text = data.decode() if isinstance(data, bytes) else data
    text = text.replace(root, "<ROOT>")
    try:
        value = json.loads(text)
    except ValueError:
        return text
    def strip(v):
        if isinstance(v, dict):
            return {k: strip(x) for k, x in v.items() if k not in VOLATILE}
        if isinstance(v, list):
            return [strip(x) for x in v]
        return v
    return json.dumps(strip(value), sort_keys=True)


def pct(b, n):
    return None if not b else round((n - b) / b * 100, 1)


def med(xs):
    return round(statistics.median(xs), 1)


def section_cold():
    root = copy("cold")
    rows = []
    for cmd in COLD:
        name = " ".join(cmd)
        outs, walls, cpus, rss = {}, {"base": [], "new": []}, {"base": [], "new": []}, {"base": [], "new": []}
        for i in range(reps):
            for label in (("base", "new") if i % 2 == 0 else ("new", "base")):
                out, w, c, r = run(BINS[label], root, cmd)
                outs.setdefault(label, out)
                walls[label].append(w); cpus[label].append(c); rss[label].append(r)
        ir = {label: instructions(BINS[label], root, cmd, f"cold-{label}") for label in BINS}
        rows.append({
            "cmd": name,
            "identical_stdout": outs["base"] == outs["new"],
            "instr": ir, "instr_pct": pct(ir["base"], ir["new"]) if callgrind else None,
            "wall_ms": {l: med(walls[l]) for l in BINS}, "wall_pct": pct(med(walls["base"]), med(walls["new"])),
            "cpu_ms": {l: med(cpus[l]) for l in BINS}, "cpu_pct": pct(med(cpus["base"]), med(cpus["new"])),
            "rss_mb": {l: med(rss[l]) for l in BINS}, "rss_pct": pct(med(rss["base"]), med(rss["new"])),
        })
        print(json.dumps(rows[-1]), flush=True)
    return rows


def procs_for(root):
    out = []
    for p in glob.glob("/proc/[0-9]*"):
        try:
            with open(p + "/cmdline", "rb") as f:
                cmd = f.read().replace(b"\0", b" ").decode()
            if root in cmd and "ab_verify" not in cmd and "valgrind" not in cmd:
                out.append(int(p.rsplit("/", 1)[1]))
        except OSError:
            pass
    return out


def cpu_ms(pids):
    tick = os.sysconf("SC_CLK_TCK"); total = 0
    for pid in pids:
        try:
            with open(f"/proc/{pid}/stat") as f:
                parts = f.read().rsplit(")", 1)[1].split()
            total += (int(parts[11]) + int(parts[12]) + int(parts[13]) + int(parts[14])) * 1000 / tick
        except OSError:
            pass
    return total


def rss_mb(pids):
    total = 0
    for pid in pids:
        try:
            with open(f"/proc/{pid}/status") as f:
                for line in f:
                    if line.startswith("VmRSS"):
                        total += int(line.split()[1]) / 1024
        except OSError:
            pass
    return round(total, 1)


class Mcp:
    def __init__(self, binary, root):
        self.p = subprocess.Popen([binary, "--root", root, "mcp"], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env)
        self.id = 0

    def send(self, method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None: msg["params"] = params
        if not notify:
            self.id += 1; msg["id"] = self.id
        self.p.stdin.write((json.dumps(msg) + "\n").encode()); self.p.stdin.flush()
        if notify: return None
        while True:
            line = self.p.stdout.readline()
            if not line: raise RuntimeError("mcp closed")
            m = json.loads(line)
            if m.get("id") == self.id: return m

    def call(self, name, arguments):
        t = time.perf_counter()
        r = self.send("tools/call", {"name": name, "arguments": arguments})
        return (time.perf_counter() - t) * 1000, r["result"]["content"][0]["text"]

    def close(self):
        self.p.stdin.close(); self.p.wait(timeout=30)


def section_mcp():
    sessions = {}
    for label in BINS:
        root = copy(f"{label}-mcp")
        m = Mcp(BINS[label], root)
        m.send("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                              "clientInfo": {"name": "ab", "version": "0"}})
        m.send("notifications/initialized", notify=True)
        tools = m.send("tools/list")["result"]["tools"]
        for name, a in MCP:  # warm-up: daemon spawn, first loads
            m.call(name, a)
        time.sleep(1.5)  # let any post-reply memory release settle
        sessions[label] = (m, root, len(json.dumps(tools)))
    rows = []
    for name, a in MCP:
        lat = {"base": [], "new": []}; texts = {}; cpu = {"base": 0.0, "new": 0.0}
        pids = {label: procs_for(sessions[label][1]) for label in BINS}
        for i in range(mcp_reps):
            for label in (("base", "new") if i % 2 == 0 else ("new", "base")):
                m, root, _ = sessions[label]
                if think_ms: time.sleep(think_ms / 1000)
                c0 = cpu_ms(pids[label])
                dt, text = m.call(name, a)
                cpu[label] += cpu_ms(pids[label]) - c0
                lat[label].append(dt); texts.setdefault(label, norm(text, root))
        cpu = {label: round(cpu[label] / mcp_reps, 2) for label in BINS}
        q = lambda xs, f: round(sorted(xs)[int(len(xs) * f)], 2)
        rows.append({
            "tool": f"{name} {json.dumps(a)}",
            "identical_result": texts["base"] == texts["new"],
            "p50_ms": {l: q(lat[l], 0.5) for l in BINS}, "p50_pct": pct(q(lat["base"], 0.5), q(lat["new"], 0.5)),
            "p90_ms": {l: q(lat[l], 0.9) for l in BINS},
            "cpu_ms_per_call": cpu, "cpu_pct": pct(cpu["base"], cpu["new"]),
            "resp_bytes": len(texts["base"]),
        })
        print(json.dumps(rows[-1]), flush=True)
    end = {}
    for label in BINS:
        m, root, schema = sessions[label]
        end[label] = {"rss_mb_all_procs": rss_mb(procs_for(root)), "tools_list_bytes": schema}
        m.close(); stop_daemon(BINS[label], root)
    print(json.dumps({"mcp_session_end": end}), flush=True)
    return {"calls": rows, "end": end}


def section_sync():
    edit = "packages/p3/src/f100.ts"
    roots = {label: copy(f"{label}-sync") for label in BINS}
    original = open(os.path.join(roots["base"], edit)).read()
    steps = [
        ("noop", None),
        ("content", original + "\n// edit\n"),
        ("structural", original + "\nexport function abAdded(): number { return 1; }\n"),
        ("restore", original),
    ]
    rows = []
    for step, text in steps:
        row = {"step": step}
        for label in BINS:
            root = roots[label]
            if text is not None:
                with open(os.path.join(root, edit), "w") as f: f.write(text)
            # callgrind run performs the sync; a second timed run of the same content is a noop,
            # so wall time comes from the callgrind-free run on a fresh edit when callgrind is off.
            if callgrind:
                row[f"instr_{label}"] = instructions(BINS[label], root, ["sync", edit], f"sync-{label}")
                _, w, c, r = run(BINS[label], root, ["sync", edit])
                row[f"followup_noop_wall_ms_{label}"] = round(w, 1)
            else:
                _, w, c, r = run(BINS[label], root, ["sync", edit])
                row[f"wall_ms_{label}"] = round(w, 1); row[f"cpu_ms_{label}"] = round(c, 1); row[f"rss_mb_{label}"] = round(r, 1)
        if callgrind:
            row["instr_pct"] = pct(row["instr_base"], row["instr_new"])
        same = all(norm(run(base, roots["base"], q)[0], roots["base"]) == norm(run(new, roots["new"], q)[0], roots["new"])
                   for q in (["context", "abAdded"], ["callers-of", "helper0_0"], ["context", "Svc3_100"]))
        row["queries_identical_after"] = same
        rows.append(row)
        print(json.dumps(row), flush=True)
    return rows


def vg_pid(root, needle):
    for p in glob.glob("/proc/[0-9]*"):
        try:
            with open(p + "/cmdline", "rb") as f:
                cmd = f.read().replace(b"\0", b" ").decode()
            if "valgrind" in cmd and root in cmd and needle in cmd:
                return int(p.rsplit("/", 1)[1])
        except OSError:
            pass
    return None


def cg_dump_ir(pid, prefix):
    subprocess.run(["callgrind_control", "-d", str(pid)], capture_output=True, timeout=120)
    files = sorted(glob.glob(f"{prefix}.{pid}.*"), key=os.path.getmtime)
    with open(files[-1]) as fh:
        for line in fh:
            if line.startswith("summary:") or line.startswith("totals:"):
                return int(line.split()[1])
    return 0


def section_warm(calls=10):
    """Instructions per call inside the warm daemon and the MCP stdio server (both under callgrind)."""
    vg = ["valgrind", "--tool=callgrind"]
    sessions = {}
    for label in BINS:
        root = copy(f"{label}-warm")
        dprefix, mprefix = os.path.join(work, f"cgw.d.{label}"), os.path.join(work, f"cgw.m.{label}")
        subprocess.Popen(vg + [f"--callgrind-out-file={dprefix}.%p", BINS[label], "--root", root, "daemon-serve"],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
        for _ in range(600):
            st = subprocess.run([BINS[label], "--root", root, "daemon", "status"], capture_output=True, text=True, env=env)
            if '"running":true' in st.stdout: break
            time.sleep(0.5)
        m = Mcp.__new__(Mcp)
        m.p = subprocess.Popen(vg + [f"--callgrind-out-file={mprefix}.%p", BINS[label], "--root", root, "mcp"],
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env)
        m.id = 0
        m.send("initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "ab", "version": "0"}})
        m.send("notifications/initialized", notify=True)
        for _ in range(2):
            for name, a in MCP: m.call(name, a)
        sessions[label] = (m, root, vg_pid(root, "daemon-serve"), m.p.pid, dprefix, mprefix)
    rows = []
    for name, a in MCP:
        row = {"tool": f"{name} {json.dumps(a)}"}
        texts = {}
        for label in BINS:
            m, root, dpid, mpid, dprefix, mprefix = sessions[label]
            # Min of 5 batches: time-driven background work (watcher wakeups, TTL'd dirty checks)
            # lands in whichever batch it happens to overlap; the minimum is the per-call cost.
            samples = []
            for _ in range(5):
                for pid in (dpid, mpid):
                    subprocess.run(["callgrind_control", "-z", str(pid)], capture_output=True, timeout=120)
                for _ in range(calls):
                    _, text = m.call(name, a)
                samples.append((cg_dump_ir(dpid, dprefix) // calls, cg_dump_ir(mpid, mprefix) // calls))
            texts[label] = norm(text, root)
            row[f"daemon_instr_per_call_{label}"] = min(d for d, _ in samples)
            row[f"mcp_instr_per_call_{label}"] = min(m_ for _, m_ in samples)
        for side in ("daemon", "mcp"):
            row[f"{side}_pct"] = pct(row[f"{side}_instr_per_call_base"], row[f"{side}_instr_per_call_new"])
        tb = row["daemon_instr_per_call_base"] + row["mcp_instr_per_call_base"]
        tn = row["daemon_instr_per_call_new"] + row["mcp_instr_per_call_new"]
        row["total_pct"] = pct(tb, tn)
        row["identical_result"] = texts["base"] == texts["new"]
        rows.append(row)
        print(json.dumps(row), flush=True)
    for label in BINS:
        m, root = sessions[label][0], sessions[label][1]
        m.close(); stop_daemon(BINS[label], root)
    for f in glob.glob(os.path.join(work, "cgw.*")):
        os.remove(f)
    return rows


result = {"base": base, "new": new, "corpus": corpus}
if "cold" in sections: result["cold"] = section_cold()
if "mcp" in sections: result["mcp"] = section_mcp()
if "sync" in sections: result["sync"] = section_sync()
if "warm" in sections: result["warm"] = section_warm()
print("RESULT " + json.dumps(result))
