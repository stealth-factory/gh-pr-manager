// gh-pr-manager - a terminal UI for GitHub pull requests
// Copyright (C) 2026 William Li
// SPDX-License-Identifier: AGPL-3.0-or-later

'use strict';

// Builds the publishable npm packages - the launcher plus one per platform -
// from a release's tarballs. Ported from opscope's npm/pack.js.
//
// No platform package sits in git. Those would have to be rewritten on
// every tag, and a hand-maintained copy is how the npm version and the
// GitHub release come to disagree. The publish job runs this against the
// same artefacts it attaches to the release, so the two cannot name
// different binaries, and the version is the tag (which release.yml has
// already checked equals Cargo.toml).

const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawnSync } = require('child_process');

const platform = require('./platform');

const LICENSE = 'AGPL-3.0-or-later';
const REPO = 'git+https://github.com/stealth-factory/gh-pr-manager.git';

function versionFromCargo(repoRoot) {
  const toml = fs.readFileSync(path.join(repoRoot, 'Cargo.toml'), 'utf8');
  const m = toml.match(/^version = "([^"]+)"/m);
  if (!m) throw new Error('Cargo.toml has no version');
  return m[1];
}

function walkFiles(dir, acc) {
  acc = acc || [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) walkFiles(full, acc);
    else acc.push(full);
  }
  return acc;
}

function findTarball(artifacts, version, rustTarget) {
  // The suffix alone is not enough: a leftover from another tag would pack
  // those binaries under this version.
  const files = walkFiles(artifacts).filter((f) => {
    const base = path.basename(f);
    return (
      base === `${platform.LAUNCHER}-v${version}-${rustTarget}.tar.gz` ||
      base === `${platform.LAUNCHER}-${version}-${rustTarget}.tar.gz`
    );
  });
  if (files.length === 0) {
    throw new Error(
      `no tarball for ${version} ${rustTarget} under ${artifacts} - the publish job cannot invent a platform the build did not produce`,
    );
  }
  if (files.length > 1) {
    throw new Error(`several tarballs for ${version} ${rustTarget}: ${files.join(', ')}`);
  }
  return files[0];
}

function extract(tarball, dest) {
  fs.mkdirSync(dest, { recursive: true });
  const r = spawnSync('tar', ['-xzf', tarball, '-C', dest], { encoding: 'utf8' });
  if (r.status !== 0) {
    throw new Error(`tar -xzf ${tarball} failed: ${r.stderr || r.stdout}`);
  }
}

function findBinary(extracted, exe) {
  // The tarball wraps a directory named after itself. Look for the binary
  // rather than assuming the wrapper's name.
  const found = walkFiles(extracted).filter((f) => path.basename(f) === exe);
  if (found.length !== 1) {
    throw new Error(`expected exactly one ${exe} in ${extracted}, found ${found.length}`);
  }
  return found[0];
}

function writeJson(file, obj) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(obj, null, 2)}\n`);
}

function copyFile(src, dest) {
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.copyFileSync(src, dest);
}

function platformManifest(p, version) {
  const manifest = {
    name: p.pkg,
    version,
    description: `${p.label} binary for ${platform.LAUNCHER}`,
    os: [p.os],
    cpu: [p.cpu],
    files: ['bin'],
    license: LICENSE,
    repository: { type: 'git', url: REPO },
    publishConfig: { access: 'public' },
  };
  if (p.libc) manifest.libc = [p.libc];
  return manifest;
}

function launcherManifest(version) {
  const template = JSON.parse(fs.readFileSync(path.join(__dirname, 'package.json'), 'utf8'));
  template.version = version;
  template.optionalDependencies = {};
  for (const p of platform.PLATFORMS) {
    template.optionalDependencies[p.pkg] = version;
  }
  // test.js is not published, so its script would be a lie on the
  // registry page.
  template.scripts = { postinstall: 'node postinstall.js' };
  return template;
}

function pack(opts) {
  const { repoRoot, version, artifacts, out } = opts;
  const cargo = versionFromCargo(repoRoot);
  if (version !== cargo) {
    throw new Error(
      `asked to pack ${version} but Cargo.toml says ${cargo} - the npm version is the release version, not a number this script invents`,
    );
  }
  const license = path.join(repoRoot, 'LICENSE');
  if (!fs.existsSync(license)) throw new Error('LICENSE is missing from the repo root');

  fs.rmSync(out, { recursive: true, force: true });
  fs.mkdirSync(out, { recursive: true });

  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'gh-pr-manager-pack-'));
  try {
    for (const p of platform.PLATFORMS) {
      const tarball = findTarball(artifacts, version, p.rust);
      const extracted = path.join(scratch, p.rust);
      extract(tarball, extracted);
      const src = findBinary(extracted, p.exe);
      const pkgDir = path.join(out, p.pkg);
      const to = path.join(pkgDir, 'bin', p.exe);
      copyFile(src, to);
      fs.chmodSync(to, 0o755);
      writeJson(path.join(pkgDir, 'package.json'), platformManifest(p, version));
      copyFile(license, path.join(pkgDir, 'LICENSE'));
    }

    const launcherDir = path.join(out, platform.LAUNCHER);
    const shim = path.join('bin', platform.LAUNCHER);
    writeJson(path.join(launcherDir, 'package.json'), launcherManifest(version));
    for (const f of ['platform.js', 'postinstall.js', 'README.md', shim]) {
      copyFile(path.join(__dirname, f), path.join(launcherDir, f));
    }
    fs.chmodSync(path.join(launcherDir, shim), 0o755);
    copyFile(license, path.join(launcherDir, 'LICENSE'));
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }

  return {
    version,
    launcher: path.join(out, platform.LAUNCHER),
    platforms: platform.PLATFORMS.map((p) => path.join(out, p.pkg)),
  };
}

function parseArgs(argv) {
  const opts = { repoRoot: path.resolve(__dirname, '..') };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--version') opts.version = argv[++i];
    else if (a === '--artifacts') opts.artifacts = argv[++i];
    else if (a === '--out') opts.out = argv[++i];
    else if (a === '--repo') opts.repoRoot = argv[++i];
    else if (a === '--help' || a === '-h') {
      process.stdout.write(
        'pack.js --version X.Y.Z --artifacts DIR --out DIR\n' +
          "Build the npm packages from a release's tarballs.\n",
      );
      process.exit(0);
    } else {
      throw new Error(`unknown argument: ${a}`);
    }
  }
  if (!opts.version) throw new Error('--version is required');
  if (!opts.artifacts) throw new Error('--artifacts is required');
  if (!opts.out) throw new Error('--out is required');
  opts.version = opts.version.replace(/^v/, '');
  opts.artifacts = path.resolve(opts.artifacts);
  opts.out = path.resolve(opts.out);
  opts.repoRoot = path.resolve(opts.repoRoot);
  return opts;
}

if (require.main === module) {
  try {
    const result = pack(parseArgs(process.argv.slice(2)));
    process.stdout.write(
      `packed ${result.version}: ${platform.LAUNCHER} + ${result.platforms.length} platforms\n`,
    );
  } catch (err) {
    console.error(err.message);
    process.exit(1);
  }
}

module.exports = { pack, parseArgs, versionFromCargo, findTarball, platformManifest, launcherManifest };
