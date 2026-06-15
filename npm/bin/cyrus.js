#!/usr/bin/env node
'use strict';
// Thin launcher for the cyrus binary. On first run it fetches the matching
// self-contained binary from the GitHub release (same artifact the curl/irm
// installers use), caches it under ~/.cyrus/bin, then execs it with your args.
// The binary itself embeds codex + cloudflared — this package stays tiny.
const fs = require('fs');
const os = require('os');
const path = require('path');
const https = require('https');
const { spawnSync, execFileSync } = require('child_process');

const REPO = process.env.CYRUS_REPO || 'dannyisbad/cyrus';
const VERSION = process.env.CYRUS_VERSION || ('v' + require('../package.json').version);

// process.platform + process.arch  ->  [rust target triple, archive ext]
const TARGETS = {
  'darwin x64': ['x86_64-apple-darwin', 'tar.gz'],
  'darwin arm64': ['aarch64-apple-darwin', 'tar.gz'],
  'linux x64': ['x86_64-unknown-linux-gnu', 'tar.gz'],
  'linux arm64': ['aarch64-unknown-linux-gnu', 'tar.gz'],
  'win32 x64': ['x86_64-pc-windows-msvc', 'zip'],
};

function die(msg) {
  process.stderr.write('cyrus: ' + msg + '\n');
  process.exit(1);
}

const key = process.platform + ' ' + process.arch;
const target = TARGETS[key];
if (!target) {
  die(`unsupported platform ${key}. Build from source: https://github.com/${REPO}`);
}
const [triple, ext] = target;
const binName = process.platform === 'win32' ? 'cyrus.exe' : 'cyrus';
const installDir = process.env.CYRUS_INSTALL_DIR || path.join(os.homedir(), '.cyrus', 'bin');
const binPath = path.join(installDir, binName);

// GET with redirect following (GitHub release assets redirect to a CDN host).
function download(url) {
  return new Promise((resolve, reject) => {
    https
      .get(url, { headers: { 'User-Agent': 'cyrus-npm', Accept: 'application/octet-stream' } }, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume();
          return resolve(download(res.headers.location));
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error(`HTTP ${res.statusCode}`));
        }
        const chunks = [];
        res.on('data', (c) => chunks.push(c));
        res.on('end', () => resolve(Buffer.concat(chunks)));
      })
      .on('error', reject);
  });
}

async function ensureBinary() {
  if (fs.existsSync(binPath)) return;
  const asset = `cyrus-${triple}.${ext}`;
  const url = `https://github.com/${REPO}/releases/download/${VERSION}/${asset}`;
  process.stderr.write(`cyrus: fetching ${asset} (${VERSION})…\n`);
  let data;
  try {
    data = await download(url);
  } catch (e) {
    die(
      `download failed (${e.message}): ${url}\n` +
        `       no release asset for ${triple} at ${VERSION}? see https://github.com/${REPO}/releases`
    );
  }
  fs.mkdirSync(installDir, { recursive: true });
  const tmp = path.join(os.tmpdir(), `cyrus-${process.pid}-${data.length}.${ext}`);
  fs.writeFileSync(tmp, data);
  try {
    // bsdtar (`tar`) extracts both .tar.gz and .zip on macOS, Linux, and Win10+.
    execFileSync('tar', ['-xf', tmp, '-C', installDir], { stdio: 'ignore' });
  } catch (e) {
    die(`could not extract ${asset} (${e.message}). Need 'tar' on PATH (macOS, Linux, Windows 10+).`);
  } finally {
    try { fs.rmSync(tmp, { force: true }); } catch (_) {}
  }
  if (!fs.existsSync(binPath)) die(`archive did not contain ${binName}.`);
  if (process.platform !== 'win32') fs.chmodSync(binPath, 0o755);
}

(async () => {
  await ensureBinary();
  const res = spawnSync(binPath, process.argv.slice(2), { stdio: 'inherit' });
  if (res.error) die(res.error.message);
  process.exit(res.status === null ? 1 : res.status);
})();
