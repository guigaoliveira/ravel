#!/usr/bin/env node
// postinstall: fetch the matching prebuilt Ravel binary from GitHub Releases into ./bin.
// Zero npm deps — uses Node built-ins + the system `tar` (present on Linux/macOS and Win10+).
//
// Env escape hatches:
//   RAVEL_GITHUB_REPO   owner/repo (default: guigaoliveira/ravel)
//   RAVEL_BINARY        path to a local ravel binary → copied instead of downloaded (dev/CI/offline)
//   RAVEL_SKIP_DOWNLOAD 1 → do nothing (e.g. building from source elsewhere)
'use strict';
const fs = require('fs');
const os = require('os');
const path = require('path');
const https = require('https');
const { spawnSync } = require('child_process');

const pkg = require('./package.json');
const REPO = process.env.RAVEL_GITHUB_REPO || 'guigaoliveira/ravel';
const BIN_DIR = path.join(__dirname, 'bin');

function log(m) { process.stderr.write(`[ravel] ${m}\n`); }
function fail(m) { log(m); process.exit(1); }

function target() {
  const p = process.platform;
  const a = process.arch;
  const arch = a === 'x64' ? 'x86_64' : a === 'arm64' ? 'aarch64' : null;
  if (!arch) fail(`unsupported CPU: ${a} — build from source: cargo install --git https://github.com/${REPO}.git ravel-cli`);
  if (p === 'linux') return { triple: `${arch}-unknown-linux-gnu`, ext: 'tar.gz', bin: 'ravel' };
  if (p === 'darwin') return { triple: `${arch}-apple-darwin`, ext: 'tar.gz', bin: 'ravel' };
  if (p === 'win32') {
    if (arch !== 'x86_64') fail('no prebuilt for windows/arm64 — build from source (cargo).');
    return { triple: `${arch}-pc-windows-msvc`, ext: 'zip', bin: 'ravel.exe' };
  }
  fail(`unsupported OS: ${p}`);
}

function download(url, dest, redirects = 0) {
  return new Promise((resolve, reject) => {
    if (redirects > 6) return reject(new Error('too many redirects'));
    https.get(url, { headers: { 'User-Agent': 'ravel-cli-npm' } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume();
        return resolve(download(res.headers.location, dest, redirects + 1));
      }
      if (res.statusCode !== 200) {
        res.resume();
        return reject(new Error(`HTTP ${res.statusCode} for ${url}`));
      }
      const file = fs.createWriteStream(dest);
      res.pipe(file);
      file.on('finish', () => file.close(() => resolve()));
      file.on('error', reject);
    }).on('error', reject);
  });
}

function extract(archive, dir) {
  // bsdtar (`tar`) handles both .tar.gz and .zip on Linux/macOS and Windows 10+.
  const r = spawnSync('tar', ['-xf', archive, '-C', dir], { stdio: 'inherit' });
  if (r.status !== 0) fail(`extraction failed (is \`tar\` on PATH?): ${r.error || r.status}`);
}

async function main() {
  if (process.env.RAVEL_SKIP_DOWNLOAD === '1') { log('RAVEL_SKIP_DOWNLOAD=1 — skipping'); return; }
  fs.mkdirSync(BIN_DIR, { recursive: true });
  const t = target();
  const outBin = path.join(BIN_DIR, t.bin);

  // Dev/offline: use a local binary instead of downloading.
  if (process.env.RAVEL_BINARY) {
    fs.copyFileSync(process.env.RAVEL_BINARY, outBin);
    if (process.platform !== 'win32') fs.chmodSync(outBin, 0o755);
    log(`installed from RAVEL_BINARY → ${outBin}`);
    return;
  }

  const asset = `ravel-${t.triple}.${t.ext}`;
  const url = `https://github.com/${REPO}/releases/download/v${pkg.version}/${asset}`;
  const tmp = path.join(os.tmpdir(), `ravel-${process.pid}-${asset}`);
  log(`downloading ${url}`);
  try {
    await download(url, tmp);
    extract(tmp, BIN_DIR);
    if (process.platform !== 'win32' && fs.existsSync(outBin)) fs.chmodSync(outBin, 0o755);
    fs.rmSync(tmp, { force: true });
    if (!fs.existsSync(outBin)) fail(`archive did not contain ${t.bin}`);
    log(`installed → ${outBin}`);
  } catch (e) {
    fail(`download failed (${e.message}). Fallback: cargo install --git https://github.com/${REPO}.git ravel-cli`);
  }
}

main();
