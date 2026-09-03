#!/usr/bin/env node
'use strict';

// donsetch postinstall: download the prebuilt binary for this platform
// from GitHub Releases, verify SHA256, and extract to ./binaries/.
//
// The binary is NOT bundled in the npm package : it's fetched at
// install time from the GitHub release matching this package version.
// This keeps the npm registry clean (no 35 MB binary tarballs) and
// uses the same release artifacts that manual users download.
//
// Supported platforms:
//   linux-x64      Linux x86_64 (glibc)
//   linux-arm64    Linux ARM64 (glibc)
//   darwin-arm64   macOS Apple Silicon
//   darwin-x64     macOS Intel
//   win32-x64      Windows x86_64

const https = require('https');
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const REPO = 'dondai44423/donsetch';
const VERSION = require('./package.json').version;
const TAG = `v${VERSION}`;

// ── platform mapping ────────────────────────────────────────────
const PLATFORMS = {
  'linux-x64':    { asset: 'donsetch-linux-x64.tar.gz',    binary: 'donsetch'     },
  'linux-arm64':  { asset: 'donsetch-linux-arm64.tar.gz',  binary: 'donsetch'     },
  'darwin-arm64': { asset: 'donsetch-darwin-arm64.tar.gz', binary: 'donsetch'     },
  'darwin-x64':   { asset: 'donsetch-darwin-x64.tar.gz',   binary: 'donsetch'     },
  'win32-x64':    { asset: 'donsetch-win32-x64.tar.gz',    binary: 'donsetch.exe' },
};

const platKey = `${process.platform}-${process.arch}`;
const plat = PLATFORMS[platKey];

if (!plat) {
  const known = {
    'darwin-x64': 'prebuilt binaries exist : update donsetch to a version that ships one',
    'win32-arm64': 'no prebuilt binary yet : build from source (see below)',
  }[platKey];
  console.error(`donsetch: unsupported platform ${platKey}${known ? ` (${known})` : ''}`);
  console.error('');
  console.error('Supported platforms:');
  console.error('  linux-x64      Linux x86_64 (glibc)');
  console.error('  linux-arm64    Linux ARM64 (glibc)');
  console.error('  darwin-arm64   macOS Apple Silicon');
  console.error('  darwin-x64     macOS Intel');
  console.error('  win32-x64      Windows x86_64');
  console.error('');
  console.error('Build from source: https://github.com/' + REPO);
  process.exit(1);
}

// ── musl detection (Alpine etc.) ────────────────────────────────
// The Linux binaries are glibc-linked. On musl systems they install
// fine but can never exec (missing ld-linux loader) : fail HERE with
// the actual cause instead of a cryptic spawn error on first run.
//
// Checking for /lib/ld-musl-*.so.1 existence is a false positive on
// glibc systems that also have musl installed as a secondary libc
// (e.g. Arch/Manjaro with the `musl` package). Instead, check what
// the RUNNING Node process actually uses: read /proc/self/exe's ELF
// interpreter (.interp section). On glibc it's ld-linux-*.so.2, on
// musl it's ld-musl-*.so.1. An env var escape hatch is also provided.
if (process.platform === 'linux') {
  // Escape hatch: force glibc path even on a musl system (user knows
  // the binary will run, e.g. via gcompat or a glibc chroot).
  if (process.env.DONSETCH_FORCE_GLIBC === '1') {
    // skip musl check
  } else {
    let isMusl = false;
    let detected = false;
    try {
      // Read the ELF interpreter of /proc/self/exe (this Node process).
      // The .interp section starts at offset 0x18 in the ELF header
      // for the program header table, but the reliable way is to parse
      // the program headers for PT_INTERP.
      const exe = '/proc/self/exe';
      const fd = fs.openSync(exe, 'r');
      // ELF header: first 64 bytes for 64-bit.
      const hdr = Buffer.alloc(64);
      fs.readSync(fd, hdr, 0, 64, 0);
      // e_phoff at offset 0x20 (64-bit), e_phentsize at 0x36, e_phnum at 0x38.
      const e_phoff = hdr.readBigUInt64LE(0x20);
      const e_phentsize = hdr.readUInt16LE(0x36);
      const e_phnum = hdr.readUInt16LE(0x38);
      for (let i = 0; i < e_phnum; i++) {
        const ph = Buffer.alloc(e_phentsize);
        fs.readSync(fd, ph, 0, e_phentsize, Number(e_phoff) + i * e_phentsize);
        // PT_INTERP = 3
        if (ph.readUInt32LE(0) === 3) {
          // p_offset at 0x08 (64-bit), p_filesz at 0x20.
          const p_offset = Number(ph.readBigUInt64LE(0x08));
          const p_filesz = ph.readBigUInt64LE(0x20);
          const interp = Buffer.alloc(Number(p_filesz));
          fs.readSync(fd, interp, 0, Number(p_filesz), p_offset);
          const loaderPath = interp.toString('ascii').replace(/\0/g, '').trim();
          isMusl = loaderPath.includes('ld-musl');
          detected = true;
          break;
        }
      }
      fs.closeSync(fd);
    } catch (_) {
      // /proc/self/exe not readable (some containers). Fall back to
      // the old existence check, but only as a last resort.
    }
    // Only fall back to the existence check if we could not read
    // the ELF interpreter at all. A successful read that found glibc
    // (isMusl=false, detected=true) must NOT fall back.
    if (!detected) {
      isMusl = fs.existsSync('/lib/ld-musl-x86_64.so.1')
        || fs.existsSync('/lib/ld-musl-aarch64.so.1');
    }
    if (isMusl) {
      console.error('donsetch: musl libc detected (Alpine?).');
      console.error('The prebuilt Linux binaries are glibc-linked and will not run.');
      console.error('');
      console.error('Options:');
      console.error('  - build from source: git clone https://github.com/' + REPO + ' && cargo build --release');
      console.error('  - use a glibc-based image/dist (debian, ubuntu, fedora)');
      console.error('  - set DONSETCH_FORCE_GLIBC=1 if you have a glibc compat layer (gcompat)');
      process.exit(1);
    }
  }
}

const binDir = path.join(__dirname, 'binaries');
const binaryPath = path.join(binDir, plat.binary);

// Skip if already installed (npm cache reuse, reinstall, etc.).
// A plausibility check first: a stale 0-byte or truncated leftover
// (killed install, crashed download) must not shadow a fresh one.
if (fs.existsSync(binaryPath)) {
  let size = 0;
  try { size = fs.statSync(binaryPath).size; } catch (_) {}
  if (size > 1024 * 1024) {
    console.log(`donsetch: binary already present (${plat.binary})`);
    process.exit(0);
  }
  console.log(`donsetch: leftover ${plat.binary} is ${size} bytes : re-downloading`);
  try { fs.unlinkSync(binaryPath); } catch (_) {}
}

fs.mkdirSync(binDir, { recursive: true });

const baseUrl = `https://github.com/${REPO}/releases/download/${TAG}`;
const assetUrl = `${baseUrl}/${plat.asset}`;
const checksumUrl = `${baseUrl}/${plat.asset}.sha256`;

// ── download with redirect following (max 5 hops) ───────────────
function download(url, dest) {
  return new Promise((resolve, reject) => {
    const MAX_REDIRECTS = 5;
    function get(u, hops) {
      if (hops > MAX_REDIRECTS) {
        reject(new Error(`too many redirects for ${url}`));
        return;
      }
      const opts = { headers: { 'Accept': 'application/octet-stream', 'User-Agent': 'donsetch-npm-installer' } };
      https.get(u, opts, (res) => {
        if ([301, 302, 303, 307, 308].includes(res.statusCode)) {
          res.resume();
          const loc = res.headers.location;
          if (!loc) { reject(new Error(`redirect without Location from ${u}`)); return; }
          // Stay on https: never follow a downgrade to cleartext.
          if (loc.startsWith('http://')) {
            reject(new Error(`refusing http:// downgrade redirect to ${loc}`));
            return;
          }
          get(loc, hops + 1);
          return;
        }
        if (res.statusCode !== 200) {
          res.resume();
          reject(new Error(`HTTP ${res.statusCode} for ${u}`));
          return;
        }
        const file = fs.createWriteStream(dest);
        res.pipe(file);
        file.on('finish', () => { file.close(); resolve(); });
        file.on('error', (err) => {
          try { fs.unlinkSync(dest); } catch (_) {}
          reject(err);
        });
      }).on('error', reject);
    }
    get(url, 0);
  });
}

// ── main ────────────────────────────────────────────────────────
async function main() {
  const tarball = path.join(binDir, plat.asset);
  const checksumFile = path.join(binDir, 'checksum.sha256');

  // 0. Windows: tar ships with Windows 10 1803+; older boxes lack it.
  //    Detect BEFORE downloading so the error names the real problem.
  if (process.platform === 'win32') {
    let hasTar = false;
    try { execFileSync('tar', ['--version'], { stdio: 'ignore' }); hasTar = true; } catch (_) {}
    if (!hasTar) {
      console.error('donsetch: `tar` not found on this Windows system.');
      console.error('tar ships with Windows 10 1803+. Update Windows, or extract manually');
      console.error('after downloading ' + assetUrl);
      process.exit(1);
    }
  }

  // 1. Download the binary tarball
  console.log(`donsetch: downloading ${plat.asset} from ${TAG}...`);
  await download(assetUrl, tarball);

  // 2. Download the SHA256 checksum
  console.log('donsetch: verifying checksum...');
  await download(checksumUrl, checksumFile);

  // 3. Verify SHA256
  const expectedHash = fs.readFileSync(checksumFile, 'utf8').trim().split(/\s+/)[0];
  const actualHash = crypto.createHash('sha256').update(fs.readFileSync(tarball)).digest('hex');

  if (actualHash !== expectedHash) {
    try { fs.unlinkSync(tarball); } catch (_) {}
    try { fs.unlinkSync(checksumFile); } catch (_) {}
    console.error(`donsetch: SHA256 mismatch!`);
    console.error(`  expected: ${expectedHash}`);
    console.error(`  actual:   ${actualHash}`);
    console.error('The download may have been corrupted or tampered with.');
    process.exit(1);
  }

  // 4. Extract (tar is built into Linux, macOS, and Windows 10+)
  console.log('donsetch: extracting...');
  // execFileSync: no shell, no string interpolation : the install
  // path (which can contain quotes/spaces) is passed as argv.
  execFileSync('tar', ['xzf', tarball, '-C', binDir], { stdio: 'inherit' });

  // 5. Cleanup
  try { fs.unlinkSync(tarball); } catch (_) {}
  try { fs.unlinkSync(checksumFile); } catch (_) {}

  // 6. Verify the binary exists after extraction (BEFORE chmod :
  //    chmod on a missing file throws an opaque error).
  if (!fs.existsSync(binaryPath)) {
    console.error(`donsetch: expected ${plat.binary} not found after extraction`);
    console.error(`  looked at: ${binaryPath}`);
    console.error('  contents of binaries/:', fs.readdirSync(binDir).join(', '));
    process.exit(1);
  }

  // 7. Make executable (Unix only)
  if (process.platform !== 'win32') {
    fs.chmodSync(binaryPath, 0o755);
  }

  console.log(`donsetch: installed ${plat.binary} to ${binaryPath}`);
  console.log(`donsetch: run \`donsetch\` to see available commands.`);
}

main().catch((err) => {
  console.error(`donsetch: install failed: ${err.message}`);
  console.error('');
  console.error('You can build from source:');
  console.error('  git clone https://github.com/' + REPO);
  console.error('  cd donsetch && cargo build --release');
  process.exit(1);
});
