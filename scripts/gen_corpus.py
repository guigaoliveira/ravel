#!/usr/bin/env python3
"""Deterministic synthetic TypeScript monorepo for ravel performance work.

Usage: gen_corpus.py <out-dir> <packages> <files-per-package>

File fN.ts imports fN-1..fN-3 in its package (the layout property_check.py expects);
every seventh file imports another package through a tsconfig `@pkg/*` alias; each
package has an index.ts barrel. Every class declares method0..method3, so member
names are shared by thousands of files the way `get` or `execute` are in real code.
`gen_corpus.py out 40 500` produces the 20,040-file corpus behind the 1.16.0 numbers.
"""
import os
import random
import sys

out, n_pkg, n_files = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
rng = random.Random(42)

os.makedirs(out, exist_ok=True)
with open(os.path.join(out, "package.json"), "w") as f:
    f.write('{"name":"synthetic","private":true,"workspaces":["packages/*"]}\n')
with open(os.path.join(out, "tsconfig.json"), "w") as f:
    f.write('{"compilerOptions":{"baseUrl":".","paths":{"@pkg/*":["packages/*/src"]}}}\n')

for p in range(n_pkg):
    pdir = os.path.join(out, "packages", f"p{p}", "src")
    os.makedirs(pdir, exist_ok=True)
    barrel = []
    for i in range(n_files):
        lines = []
        # local imports
        for d in (1, 2, 3):
            if i - d >= 0:
                lines.append(f"import {{ Svc{p}_{i-d}, helper{p}_{i-d} }} from './f{i-d}';")
        # cross-package import
        if p > 0 and i % 7 == 0:
            q = rng.randrange(p)
            j = rng.randrange(n_files)
            lines.append(f"import {{ Svc{q}_{j} }} from '@pkg/p{q}/f{j}';")
        lines.append(f"import type {{ Shape{p}_{max(i-1,0)} }} from './f{max(i-1,0)}';")
        lines.append("")
        lines.append(f"export interface Shape{p}_{i} {{ id: string; value: number; next?: Shape{p}_{max(i-1,0)} }}")
        lines.append(f"export type Alias{p}_{i} = Shape{p}_{i} | null;")
        lines.append("")
        lines.append(f"export function helper{p}_{i}(x: number, y: number): number {{")
        lines.append("  let acc = 0;")
        lines.append("  for (let k = 0; k < x; k++) {")
        lines.append("    if (k % 2 === 0 && y > 0) { acc += k; } else if (k % 3 === 0) { acc -= y; }")
        lines.append("  }")
        if i >= 1:
            lines.append(f"  return acc + helper{p}_{i-1}(x - 1, y);")
        else:
            lines.append("  return acc;")
        lines.append("}")
        lines.append("")
        base = f" extends Svc{p}_{i-1}" if i >= 1 and i % 5 != 0 else ""
        lines.append(f"export class Svc{p}_{i}{base} {{")
        lines.append(f"  private readonly name = 'svc{p}_{i}';")
        lines.append("  constructor(private readonly dep?: unknown) {")
        if base:
            lines.append("    super(dep);")
        lines.append("  }")
        for m in range(4):
            lines.append(f"  method{m}(input: Shape{p}_{i}): Alias{p}_{i} {{")
            lines.append(f"    const r = helper{p}_{i}(input.value, {m});")
            if i >= 2:
                lines.append(f"    const other = new Svc{p}_{i-2}();")
                lines.append(f"    other.method{m}(input as any);")
            lines.append("    switch (r) { case 0: return null; case 1: return input; default: break; }")
            lines.append("    return r > 10 ? input : null;")
            lines.append("  }")
        lines.append("}")
        lines.append("")
        lines.append(f"export const CONST_{p}_{i} = {{ a: 1, b: '{i}', c: [1, 2, 3] }};")
        lines.append(f"export default Svc{p}_{i};")
        with open(os.path.join(pdir, f"f{i}.ts"), "w") as f:
            f.write("\n".join(lines) + "\n")
        barrel.append(f"export * from './f{i}';")
    with open(os.path.join(pdir, "index.ts"), "w") as f:
        f.write("\n".join(barrel) + "\n")
