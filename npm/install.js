#!/usr/bin/env node
// postinstall: fetch the matching prebuilt Ravel binary from GitHub Releases into ./bin.
// Zero npm deps — download and extraction use Node built-ins only.
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
const zlib = require('zlib');
const crypto = require('crypto');

const pkg = require('./package.json');
const REPO = process.env.RAVEL_GITHUB_REPO || 'guigaoliveira/ravel';
const BIN_DIR = path.join(__dirname, 'bin');
const MAX_ARCHIVE_BYTES = Number(process.env.RAVEL_MAX_ARCHIVE_BYTES || 128 * 1024 * 1024);
const MAX_EXTRACTED_BYTES = Number(process.env.RAVEL_MAX_EXTRACTED_BYTES || 256 * 1024 * 1024);
const MAX_ARCHIVE_ENTRIES = Number(process.env.RAVEL_MAX_ARCHIVE_ENTRIES || 128);
if (![MAX_ARCHIVE_BYTES, MAX_EXTRACTED_BYTES, MAX_ARCHIVE_ENTRIES]
  .every((value) => Number.isSafeInteger(value) && value > 0)) fail('archive limits must be positive integers');

function log(m) { process.stderr.write(`[ravel] ${m}\n`); }
function fail(m) { log(m); process.exit(1); }

function target(p = process.platform, a = process.arch, report = process.report) {
  const arch = a === 'x64' ? 'x86_64' : a === 'arm64' ? 'aarch64' : null;
  if (!arch) fail(`unsupported CPU: ${a} — build from source: cargo install --git https://github.com/${REPO}.git ravel-cli`);
  if (p === 'linux') {
    const musl = report && report.getReport().header.glibcVersionRuntime === undefined;
    return { triple: `${arch}-unknown-linux-${musl ? 'musl' : 'gnu'}`, ext: 'tar.gz', bin: 'ravel' };
  }
  if (p === 'darwin') return { triple: `${arch}-apple-darwin`, ext: 'tar.gz', bin: 'ravel' };
  if (p === 'win32') return { triple: `${arch}-pc-windows-msvc`, ext: 'zip', bin: 'ravel.exe' };
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
      let received = 0;
      res.on('data', (chunk) => {
        received += chunk.length;
        if (received > MAX_ARCHIVE_BYTES) res.destroy(new Error('archive exceeds configured download limit'));
      });
      res.pipe(file);
      res.on('error', reject);
      file.on('finish', () => file.close(() => resolve()));
      file.on('error', reject);
    }).on('error', reject);
  });
}

function safeOutput(dir, name) {
  const normalized = name.replace(/\\/g, '/');
  if (normalized.startsWith('/') || normalized.split('/').includes('..')) fail(`unsafe archive path: ${name}`);
  const out = path.resolve(dir, normalized);
  if (out !== path.resolve(dir) && !out.startsWith(path.resolve(dir) + path.sep)) fail(`unsafe archive path: ${name}`);
  return out;
}

function crc32(buffer) {
  let crc = 0xffffffff;
  for (const byte of buffer) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function extractTarGz(archive, dir) {
  const tar = zlib.gunzipSync(fs.readFileSync(archive), { maxOutputLength: MAX_EXTRACTED_BYTES });
  let entries = 0;
  for (let offset = 0; offset + 512 <= tar.length;) {
    const header = tar.subarray(offset, offset + 512);
    if (header.every((byte) => byte === 0)) break;
    if (++entries > MAX_ARCHIVE_ENTRIES) fail('archive has too many entries');
    const expectedChecksum = parseInt(header.subarray(148, 156).toString('ascii').replace(/\0.*$/, '').trim(), 8);
    const checksumHeader = Buffer.from(header);
    checksumHeader.fill(32, 148, 156);
    const actualChecksum = checksumHeader.reduce((sum, byte) => sum + byte, 0);
    if (!Number.isFinite(expectedChecksum) || expectedChecksum !== actualChecksum) fail('invalid tar header checksum');
    const name = header.subarray(0, 100).toString('utf8').replace(/\0.*$/, '');
    const size = parseInt(header.subarray(124, 136).toString('ascii').replace(/\0.*$/, '').trim() || '0', 8);
    const type = header[156];
    offset += 512;
    if (!Number.isSafeInteger(size) || size < 0 || offset + size > tar.length) fail('truncated tar entry');
    const out = safeOutput(dir, name);
    if (type === 0 || type === 48) {
      fs.mkdirSync(path.dirname(out), { recursive: true });
      fs.writeFileSync(out, tar.subarray(offset, offset + size));
    } else if (type === 53) {
      fs.mkdirSync(out, { recursive: true });
    }
    offset += Math.ceil(size / 512) * 512;
  }
}

function extractZip(archive, dir) {
  const zip = fs.readFileSync(archive);
  let eocd = -1;
  for (let i = zip.length - 22; i >= Math.max(0, zip.length - 65557); i--) {
    if (zip.readUInt32LE(i) === 0x06054b50) { eocd = i; break; }
  }
  if (eocd < 0) fail('invalid zip: end record not found');
  const entries = zip.readUInt16LE(eocd + 10);
  if (entries > MAX_ARCHIVE_ENTRIES) fail('archive has too many entries');
  let offset = zip.readUInt32LE(eocd + 16);
  let extractedBytes = 0;
  for (let entry = 0; entry < entries; entry++) {
    if (offset < 0 || offset + 46 > zip.length) fail('truncated zip directory');
    if (zip.readUInt32LE(offset) !== 0x02014b50) fail('invalid zip directory');
    const flags = zip.readUInt16LE(offset + 8);
    if (flags & 1) fail('encrypted zip entries are unsupported');
    const method = zip.readUInt16LE(offset + 10);
    const expectedCrc = zip.readUInt32LE(offset + 16);
    const compressedSize = zip.readUInt32LE(offset + 20);
    const uncompressedSize = zip.readUInt32LE(offset + 24);
    const nameLength = zip.readUInt16LE(offset + 28);
    const extraLength = zip.readUInt16LE(offset + 30);
    const commentLength = zip.readUInt16LE(offset + 32);
    const localOffset = zip.readUInt32LE(offset + 42);
    if ([compressedSize, uncompressedSize, localOffset].includes(0xffffffff)) fail('zip64 archives are unsupported');
    extractedBytes += uncompressedSize;
    if (extractedBytes > MAX_EXTRACTED_BYTES) fail('archive exceeds configured extraction limit');
    if (offset + 46 + nameLength + extraLength + commentLength > zip.length) fail('truncated zip directory entry');
    const name = zip.subarray(offset + 46, offset + 46 + nameLength).toString('utf8');
    if (localOffset + 30 > zip.length || zip.readUInt32LE(localOffset) !== 0x04034b50) fail('invalid zip local entry');
    const localNameLength = zip.readUInt16LE(localOffset + 26);
    const localExtraLength = zip.readUInt16LE(localOffset + 28);
    const start = localOffset + 30 + localNameLength + localExtraLength;
    if (start + compressedSize > zip.length) fail('truncated zip entry');
    const data = zip.subarray(start, start + compressedSize);
    const out = safeOutput(dir, name);
    if (name.endsWith('/')) {
      fs.mkdirSync(out, { recursive: true });
    } else {
      fs.mkdirSync(path.dirname(out), { recursive: true });
      const contents = method === 0 ? data : method === 8 ? zlib.inflateRawSync(data) : fail(`unsupported zip method: ${method}`);
      if (contents.length !== uncompressedSize || crc32(contents) !== expectedCrc) fail(`corrupt zip entry: ${name}`);
      fs.writeFileSync(out, contents);
    }
    offset += 46 + nameLength + extraLength + commentLength;
  }
}

function verifySha256(file, checksums, asset) {
  const escaped = asset.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const match = checksums.match(new RegExp(`^([a-fA-F0-9]{64})\\s+[*]?${escaped}$`, 'm'));
  if (!match) fail(`checksum missing for ${asset}`);
  const actual = crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
  if (actual.toLowerCase() !== match[1].toLowerCase()) fail(`checksum mismatch for ${asset}`);
}

function extract(archive, dir, ext) {
  if (fs.statSync(archive).size > MAX_ARCHIVE_BYTES) fail('archive exceeds configured download limit');
  if (ext === 'tar.gz') extractTarGz(archive, dir);
  else extractZip(archive, dir);
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
  const checksumTmp = `${tmp}.sha256sums`;
  log(`downloading ${url}`);
  try {
    await download(url, tmp);
    await download(`https://github.com/${REPO}/releases/download/v${pkg.version}/SHA256SUMS`, checksumTmp);
    verifySha256(tmp, fs.readFileSync(checksumTmp, 'utf8'), asset);
    extract(tmp, BIN_DIR, t.ext);
    if (process.platform !== 'win32' && fs.existsSync(outBin)) fs.chmodSync(outBin, 0o755);
    fs.rmSync(tmp, { force: true });
    fs.rmSync(checksumTmp, { force: true });
    if (!fs.existsSync(outBin)) fail(`archive did not contain ${t.bin}`);
    log(`installed → ${outBin}`);
  } catch (e) {
    fs.rmSync(tmp, { force: true });
    fs.rmSync(checksumTmp, { force: true });
    fail(`download failed (${e.message}). Fallback: cargo install --git https://github.com/${REPO}.git ravel-cli`);
  }
}

if (require.main === module) main();

module.exports = { extract, target, verifySha256 };
