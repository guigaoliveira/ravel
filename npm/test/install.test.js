'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');
const zlib = require('node:zlib');
const { extract, target, verifySha256 } = require('../install');

function crc32(buffer) {
  let crc = 0xffffffff;
  for (const byte of buffer) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function storedZip(name, data) {
  const filename = Buffer.from(name);
  const crc = crc32(data);
  const local = Buffer.alloc(30);
  local.writeUInt32LE(0x04034b50, 0); local.writeUInt16LE(20, 4);
  local.writeUInt32LE(crc, 14); local.writeUInt32LE(data.length, 18); local.writeUInt32LE(data.length, 22);
  local.writeUInt16LE(filename.length, 26);
  const central = Buffer.alloc(46);
  central.writeUInt32LE(0x02014b50, 0); central.writeUInt16LE(20, 4); central.writeUInt16LE(20, 6);
  central.writeUInt32LE(crc, 16); central.writeUInt32LE(data.length, 20); central.writeUInt32LE(data.length, 24);
  central.writeUInt16LE(filename.length, 28);
  const centralOffset = local.length + filename.length + data.length;
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0); eocd.writeUInt16LE(1, 8); eocd.writeUInt16LE(1, 10);
  eocd.writeUInt32LE(central.length + filename.length, 12); eocd.writeUInt32LE(centralOffset, 16);
  return Buffer.concat([local, filename, data, central, filename, eocd]);
}

function expectInstallerFailure(fn, pattern) {
  const originalExit = process.exit;
  process.exit = () => { throw new Error('blocked'); };
  try { assert.throws(fn, pattern); }
  finally { process.exit = originalExit; }
}

function tarHeader(name, data) {
  const header = Buffer.alloc(512);
  header.write(name, 0);
  header.write('0000755\0', 100);
  header.write(`${data.length.toString(8).padStart(11, '0')}\0`, 124);
  header.fill(32, 148, 156);
  header[156] = 48;
  header.write('ustar\0', 257);
  const checksum = header.reduce((sum, byte) => sum + byte, 0);
  header.write(`${checksum.toString(8).padStart(6, '0')}\0 `, 148);
  return Buffer.concat([header, data, Buffer.alloc((512 - data.length % 512) % 512), Buffer.alloc(1024)]);
}

test('extracts a valid tar.gz and rejects corruption', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ravel-install-'));
  const archive = path.join(tmp, 'ravel.tar.gz');
  fs.writeFileSync(archive, zlib.gzipSync(tarHeader('ravel', Buffer.from('binary'))));
  extract(archive, path.join(tmp, 'out'), 'tar.gz');
  assert.equal(fs.readFileSync(path.join(tmp, 'out/ravel'), 'utf8'), 'binary');
  const corrupt = zlib.gunzipSync(fs.readFileSync(archive));
  corrupt[0] ^= 1;
  fs.writeFileSync(archive, zlib.gzipSync(corrupt));
  expectInstallerFailure(() => extract(archive, path.join(tmp, 'bad'), 'tar.gz'), /blocked/);
});

test('rejects tar path traversal', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ravel-install-'));
  const archive = path.join(tmp, 'bad.tar.gz');
  fs.writeFileSync(archive, zlib.gzipSync(tarHeader('../escape', Buffer.alloc(0))));
  expectInstallerFailure(() => extract(archive, path.join(tmp, 'out'), 'tar.gz'), /blocked/);
});

test('extracts zip and validates CRC', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ravel-install-'));
  const archive = path.join(tmp, 'ravel.zip');
  fs.writeFileSync(archive, storedZip('ravel.exe', Buffer.from('binary')));
  extract(archive, path.join(tmp, 'out'), 'zip');
  assert.equal(fs.readFileSync(path.join(tmp, 'out/ravel.exe'), 'utf8'), 'binary');
  const corrupt = fs.readFileSync(archive); corrupt[40] ^= 1; fs.writeFileSync(archive, corrupt);
  expectInstallerFailure(() => extract(archive, path.join(tmp, 'bad'), 'zip'), /blocked/);
});

test('rejects zip path traversal', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ravel-install-'));
  const archive = path.join(tmp, 'bad.zip');
  fs.writeFileSync(archive, storedZip('../escape.exe', Buffer.from('binary')));
  expectInstallerFailure(() => extract(archive, path.join(tmp, 'out'), 'zip'), /blocked/);
});

test('maps every published platform target', () => {
  const glibc = { getReport: () => ({ header: { glibcVersionRuntime: '2.34' } }) };
  const musl = { getReport: () => ({ header: {} }) };
  assert.equal(target('linux', 'x64', glibc).triple, 'x86_64-unknown-linux-gnu');
  assert.equal(target('linux', 'arm64', glibc).triple, 'aarch64-unknown-linux-gnu');
  assert.equal(target('linux', 'x64', musl).triple, 'x86_64-unknown-linux-musl');
  assert.equal(target('linux', 'arm64', musl).triple, 'aarch64-unknown-linux-musl');
  assert.equal(target('darwin', 'x64').triple, 'x86_64-apple-darwin');
  assert.equal(target('darwin', 'arm64').triple, 'aarch64-apple-darwin');
  assert.equal(target('win32', 'x64').triple, 'x86_64-pc-windows-msvc');
  assert.equal(target('win32', 'arm64').triple, 'aarch64-pc-windows-msvc');
});

test('verifies release checksum and rejects mismatch', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ravel-install-'));
  const asset = path.join(tmp, 'ravel.tar.gz');
  fs.writeFileSync(asset, 'artifact');
  const digest = require('node:crypto').createHash('sha256').update('artifact').digest('hex');
  verifySha256(asset, `${digest}  ravel.tar.gz\n`, 'ravel.tar.gz');
  expectInstallerFailure(
    () => verifySha256(asset, `${'0'.repeat(64)}  ravel.tar.gz\n`, 'ravel.tar.gz'),
    /blocked/,
  );
});
