#!/usr/bin/env python3
"""Drive `ravel mcp` over stdio the way an agent does and sample the RSS of every
ravel process (stdio server + shared daemon) after each step.

Usage: mcp_session_bench.py <ravel-bin> <indexed-root> <query,query,...> [<file-to-edit>]

With a file, three exported functions are appended to it one at a time (each
followed by the `sync` tool and an `explore` of the new name), the file is restored
and synced, every query is run again, and each warm daemon answer is compared with a
cold `ravel context` on the same tree. The root should be indexed first. Numbers
belong to this machine; compare two binaries on the same root.
"""
import json, os, subprocess, sys, time, glob

def rss_tree(root_pid):
    """Sum of RSS (MB) for root_pid and all descendants, plus per-process list."""
    procs = {}
    for p in glob.glob('/proc/[0-9]*'):
        try:
            with open(p + '/stat') as f:
                parts = f.read().rsplit(')', 1)
                pid = int(parts[0].split('(')[0]); comm = parts[0].split('(',1)[1]
                ppid = int(parts[1].split()[1])
            with open(p + '/status') as f:
                rss = 0
                for line in f:
                    if line.startswith('VmRSS'):
                        rss = int(line.split()[1]) / 1024
            procs[pid] = (ppid, comm, rss)
        except Exception:
            pass
    out = []
    def walk(pid):
        for cpid, (ppid, comm, rss) in procs.items():
            if ppid == pid:
                out.append((cpid, comm, rss)); walk(cpid)
    if root_pid in procs:
        out.append((root_pid, procs[root_pid][1], procs[root_pid][2]))
        walk(root_pid)
    return out

def all_ravel():
    out = []
    for p in glob.glob('/proc/[0-9]*'):
        try:
            with open(p + '/cmdline', 'rb') as f: cmd = f.read().replace(b'\0', b' ').decode()
            if 'ravel' not in cmd or 'mcp_session' in cmd: continue
            with open(p + '/status') as f:
                rss = 0
                for line in f:
                    if line.startswith('VmRSS'): rss = int(line.split()[1]) / 1024
            out.append((int(p.split('/')[-1]), cmd.strip()[:60], rss))
        except Exception: pass
    return out

class Mcp:
    def __init__(self, binary, root, env=None):
        e = dict(os.environ); e.update(env or {})
        self.p = subprocess.Popen([binary, '--root', root, 'mcp'], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=open(root + '/.mcp.stderr', 'w'), env=e)
        self.id = 0
    def send(self, method, params=None, notify=False):
        msg = {'jsonrpc': '2.0', 'method': method}
        if params is not None: msg['params'] = params
        if not notify:
            self.id += 1; msg['id'] = self.id
        self.p.stdin.write((json.dumps(msg) + '\n').encode()); self.p.stdin.flush()
        if notify: return None
        while True:
            line = self.p.stdout.readline()
            if not line: raise RuntimeError('mcp closed')
            m = json.loads(line)
            if m.get('id') == self.id: return m
    def call(self, name, args):
        t = time.perf_counter()
        r = self.send('tools/call', {'name': name, 'arguments': args})
        dt = (time.perf_counter() - t) * 1000
        text = r['result']['content'][0]['text']
        return dt, text
    def close(self):
        self.p.stdin.close(); self.p.wait(timeout=30)

def report(label, mcp, dt=None, text=None):
    procs = all_ravel()
    total = sum(r for _,_,r in procs)
    s = f'{label:40s}'
    if dt is not None: s += f' {dt:8.1f} ms'
    else: s += ' ' * 12
    s += f'  rss_total={total:7.1f} MB  ' + '  '.join(f'[{c.split()[-1] if c else "?"}:{r:.0f}]' for _,c,r in procs)
    if text is not None: s += f'  resp={len(text)}B'
    print(s, flush=True)

if __name__ == '__main__':
    binary, root = sys.argv[1], sys.argv[2]
    queries = sys.argv[3].split(',')
    edit_file = sys.argv[4] if len(sys.argv) > 4 else None
    mcp = Mcp(binary, root)
    r = mcp.send('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {}, 'clientInfo': {'name': 'bench', 'version': '0'}})
    mcp.send('notifications/initialized', notify=True)
    time.sleep(0.3)
    report('initialize', mcp)
    tools = mcp.send('tools/list')['result']['tools']
    print('tools:', [t['name'] for t in tools], 'schema bytes:', len(json.dumps(tools)))
    dt, text = mcp.call('status', {})
    report('status', mcp, dt, text)
    for q in queries:
        dt, text = mcp.call('explore', {'query': q})
        report(f'explore {q}', mcp, dt, text)
    for q in queries[:2]:
        dt, text = mcp.call('callers_of', {'node': q})
        report(f'callers_of {q}', mcp, dt, text)
    if edit_file:
        path = os.path.join(root, edit_file)
        orig = open(path).read()
        for i in range(3):
            with open(path, 'a') as f: f.write(f'\nexport function benchEdit{i}() {{ return {i}; }}\n')
            dt, text = mcp.call('sync', {'paths': [edit_file]})
            report(f'sync structural #{i}', mcp, dt, text)
            dt, text = mcp.call('explore', {'query': f'benchEdit{i}'})
            report(f'explore benchEdit{i}', mcp, dt, text)
        open(path, 'w').write(orig)
        dt, text = mcp.call('sync', {'paths': [edit_file]})
        report('sync restore', mcp, dt, text)
        for q in queries[:3]:
            dt, text = mcp.call('explore', {'query': q})
            report(f'explore {q} (after syncs)', mcp, dt, text)
    if edit_file:
        import subprocess
        mismatches = 0
        for q in queries:
            dt, text = mcp.call('explore', {'query': q})
            warm = json.loads(text); warm.pop('sid', None); warm.pop('auto_synced', None)
            cold_out = subprocess.run([binary, '--root', root, 'context', q, '--limit', '10'], capture_output=True, text=True).stdout
            cold = json.loads(cold_out); cold.pop('sid', None); cold.pop('auto_synced', None)
            if warm != cold:
                mismatches += 1
                print(f'  WARM != COLD for {q}: warm keys {sorted(warm)[:5]} cold keys {sorted(cold)[:5]}')
        print(f'warm-vs-cold check: {len(queries)} queries, {mismatches} mismatches', flush=True)
    time.sleep(1)
    report('idle 1s', mcp)
    for pid, cmd, rss in all_ravel():
        if rss > 50:
            st = open(f'/proc/{pid}/status').read()
            print('  daemon pid', pid, {k: v.strip() for k, v in (l.split(':',1) for l in st.splitlines()) if k in ('VmRSS','RssAnon','RssFile','RssShmem','VmHWM','Threads')})
            maps = open(f'/proc/{pid}/maps').read().splitlines()
            packs = [m for m in maps if '.pack' in m]
            print('  mapped pack files:', len(packs), sorted(set(m.split()[-1].split('/')[-1][:40] for m in packs)))
    mcp.close()
    time.sleep(1.5)
    report('after close', mcp)
