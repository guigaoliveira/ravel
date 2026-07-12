#!/usr/bin/env node
// Thin shim: exec the native Ravel binary fetched by install.js, forwarding argv + exit code.
'use strict';
const path = require('path');
const fs = require('fs');
const { spawnSync } = require('child_process');

const native = path.join(__dirname, process.platform === 'win32' ? 'ravel.exe' : 'ravel');
if (!fs.existsSync(native)) {
  process.stderr.write(
    '[ravel] native binary missing — the postinstall step did not run ' +
    '(installed with --ignore-scripts?).\n' +
    '        Reinstall without --ignore-scripts, or run: node ' +
    path.join(__dirname, '..', 'install.js') + '\n'
  );
  process.exit(1);
}
const r = spawnSync(native, process.argv.slice(2), { stdio: 'inherit' });
if (r.error) { process.stderr.write(`[ravel] ${r.error.message}\n`); process.exit(1); }
process.exit(r.status === null ? 1 : r.status);
