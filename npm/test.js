// gh-pr-manager - a terminal UI for GitHub pull requests
// Copyright (C) 2026 William Li
// SPDX-License-Identifier: AGPL-3.0-or-later

'use strict';

// Run with `node --test npm/test.js`. CI runs it on every pull request.

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawnSync } = require('child_process');

const platform = require('./platform');
const packer = require('./pack.js');
const stale = require('./refuse-stale-latest.js');

const repoRoot = path.resolve(__dirname, '..');

function rustTargetsFromReleaseYml() {
  const yml = fs.readFileSync(path.join(repoRoot, '.github/workflows/release.yml'), 'utf8');
  return [...yml.matchAll(/^\s+- target: ([a-z0-9_-]+)$/gm)].map((m) => m[1]);
}

function scratch() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'gh-pr-manager-npm-'));
}

function makeTarball(dir, p, tag) {
  const name = `${platform.LAUNCHER}-${tag}-${p.rust}`;
  const root = path.join(dir, name);
  fs.mkdirSync(root, { recursive: true });
  const file = path.join(root, p.exe);
  fs.writeFileSync(file, `#!/bin/sh\necho ${platform.LAUNCHER} ${tag}\n`);
  fs.chmodSync(file, 0o755);
  fs.writeFileSync(path.join(root, 'README.md'), 'not a binary');
  const r = spawnSync('tar', ['-czf', `${name}.tar.gz`, name], { cwd: dir, encoding: 'utf8' });
  assert.equal(r.status, 0, r.stderr);
}

test('every release target has an npm platform, and no extra ones', () => {
  assert.deepEqual(
    platform.PLATFORMS.map((p) => p.rust).sort(),
    rustTargetsFromReleaseYml().sort(),
    'platform.js and release.yml have drifted - a target without a package publishes a tarball nobody can npx',
  );
});

test('package.json lists exactly the platform packages', () => {
  const manifest = require('./package.json');
  assert.equal(manifest.name, platform.LAUNCHER);
  assert.deepEqual(Object.keys(manifest.bin), [platform.LAUNCHER]);
  assert.deepEqual(
    Object.keys(manifest.optionalDependencies).sort(),
    platform.PLATFORMS.map((p) => p.pkg).sort(),
  );
});

test('each published host resolves to its own package', () => {
  const cases = [
    [{ os: 'linux', cpu: 'x64', libc: 'glibc' }, 'gh-pr-manager-linux-x64'],
    [{ os: 'linux', cpu: 'arm64', libc: 'glibc' }, 'gh-pr-manager-linux-arm64'],
    [{ os: 'darwin', cpu: 'arm64' }, 'gh-pr-manager-darwin-arm64'],
    [{ os: 'darwin', cpu: 'x64' }, 'gh-pr-manager-darwin-x64'],
    [{ os: 'win32', cpu: 'x64' }, 'gh-pr-manager-win32-x64'],
  ];
  for (const [h, pkg] of cases) assert.equal(platform.currentPlatform(h).pkg, pkg);
  assert.equal(platform.currentPlatform({ os: 'win32', cpu: 'x64' }).exe, 'gh-pr-manager.exe');
});

test('musl, 32-bit, Windows arm64 and old glibc match nothing', () => {
  assert.equal(platform.currentPlatform({ os: 'linux', cpu: 'x64', libc: 'musl' }), null);
  assert.equal(platform.currentPlatform({ os: 'linux', cpu: 'ia32', libc: 'glibc' }), null);
  assert.equal(platform.currentPlatform({ os: 'win32', cpu: 'arm64' }), null);
  assert.equal(
    platform.currentPlatform({ os: 'linux', cpu: 'x64', libc: 'glibc', glibc: '2.31' }),
    null,
  );
  assert.equal(platform.glibcAtLeast('2.35', platform.MIN_GLIBC), true);
  assert.equal(platform.glibcAtLeast('3.0', platform.MIN_GLIBC), true);
  const msg = platform.unsupportedMessage({ os: 'linux', cpu: 'x64', libc: 'glibc', glibc: '2.31' });
  assert.match(msg, /glibc 2\.35/);
  assert.match(msg, /2\.31/);
});

test('the unsupported sentence names every published platform', () => {
  const msg = platform.unsupportedMessage({ os: 'freebsd', cpu: 'x64' });
  assert.match(msg, /freebsd-x64/);
  for (const p of platform.PLATFORMS) {
    assert.match(msg, new RegExp(p.label.replace(/[()]/g, '\\$&')));
  }
});

test('pack.js stamps one version onto every package', () => {
  const dir = scratch();
  try {
    const version = packer.versionFromCargo(repoRoot);
    const artifacts = path.join(dir, 'artifacts');
    fs.mkdirSync(artifacts);
    for (const p of platform.PLATFORMS) makeTarball(artifacts, p, `v${version}`);
    const result = packer.pack({ repoRoot, version, artifacts, out: path.join(dir, 'out') });
    assert.equal(result.platforms.length, platform.PLATFORMS.length);

    const launcher = JSON.parse(fs.readFileSync(path.join(result.launcher, 'package.json'), 'utf8'));
    assert.equal(launcher.version, version);
    assert.deepEqual(launcher.scripts, { postinstall: 'node postinstall.js' });
    for (const p of platform.PLATFORMS) assert.equal(launcher.optionalDependencies[p.pkg], version);
    for (const f of ['bin/gh-pr-manager', 'platform.js', 'postinstall.js', 'README.md', 'LICENSE']) {
      assert.ok(fs.existsSync(path.join(result.launcher, f)), `launcher missing ${f}`);
    }

    platform.PLATFORMS.forEach((p, i) => {
      const manifest = JSON.parse(fs.readFileSync(path.join(result.platforms[i], 'package.json'), 'utf8'));
      assert.equal(manifest.name, p.pkg);
      assert.equal(manifest.version, version);
      assert.deepEqual(manifest.os, [p.os]);
      assert.deepEqual(manifest.cpu, [p.cpu]);
      if (p.libc) assert.deepEqual(manifest.libc, [p.libc]);
      else assert.equal(manifest.libc, undefined);
      const bin = path.join(result.platforms[i], 'bin', p.exe);
      assert.ok(fs.existsSync(bin), `${p.pkg} missing ${p.exe}`);
      if (process.platform !== 'win32') assert.ok(fs.statSync(bin).mode & 0o100);
    });
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('pack.js refuses a version that is not the manifest', () => {
  assert.throws(
    () => packer.pack({ repoRoot, version: '9.9.9', artifacts: repoRoot, out: path.join(os.tmpdir(), 'nope') }),
    /Cargo\.toml says/,
  );
});

test('pack.js refuses a leftover tarball from another version, or a missing platform', () => {
  const dir = scratch();
  try {
    const version = packer.versionFromCargo(repoRoot);
    const leftover = version === '0.0.0' ? '0.0.1' : '0.0.0';
    const artifacts = path.join(dir, 'artifacts');
    fs.mkdirSync(artifacts);
    for (const p of platform.PLATFORMS) makeTarball(artifacts, p, `v${leftover}`);
    assert.throws(() => packer.pack({ repoRoot, version, artifacts, out: path.join(dir, 'out') }), /no tarball/);
    makeTarball(artifacts, platform.PLATFORMS[0], `v${version}`);
    assert.throws(() => packer.pack({ repoRoot, version, artifacts, out: path.join(dir, 'out') }), /no tarball/);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('refuse-stale-latest refuses to move latest backwards', () => {
  const names = stale.packageNames();
  assert.equal(names.length, platform.PLATFORMS.length + 1);
  assert.equal(stale.refuseStaleLatest({ version: '0.2.0', names, view: () => null }), null);
  assert.equal(stale.refuseStaleLatest({ version: '0.2.0', names, view: () => '0.1.0' }), null);
  assert.match(stale.refuseStaleLatest({ version: '0.2.0', names, view: () => '0.3.0' }), /newer publish/);
  assert.match(stale.refuseStaleLatest({ version: '0.2.0', names, view: () => 'garbage' }), /not x\.y\.z/);
});

test('refuse-stale-latest treats only E404 as unpublished and fails closed otherwise', () => {
  const failing = (stderr) => () => {
    const err = new Error('Command failed: npm view');
    err.stderr = stderr;
    throw err;
  };
  assert.equal(stale.viewLatest('x', () => '0.1.0\n'), '0.1.0');
  assert.equal(stale.viewLatest('x', failing('npm error code E404\nnpm error 404 Not Found')), null);
  assert.throws(() => stale.viewLatest('x', failing('npm error code ETIMEDOUT')), /npm view x failed: npm error code ETIMEDOUT/);
  assert.throws(() => stale.viewLatest('x', failing('npm error code E503')), /E503/);
  const errors = [];
  const orig = console.error;
  console.error = (m) => errors.push(m);
  try {
    const view = (name) => stale.viewLatest(name, failing('npm error code E500'));
    assert.equal(stale.main(['node', 'refuse-stale-latest.js', '0.2.0'], view), 1);
  } finally {
    console.error = orig;
  }
  assert.match(errors.join('\n'), /E500/);
});

test('the shim execs the binary from the matching platform package and passes the exit code', {
  skip: process.platform === 'win32' || !platform.currentPlatform(),
}, () => {
  const wanted = platform.currentPlatform();
  const dir = scratch();
  try {
    const modules = path.join(dir, 'node_modules');
    const launcherDir = path.join(modules, platform.LAUNCHER);
    const platDir = path.join(modules, wanted.pkg);
    fs.mkdirSync(path.join(launcherDir, 'bin'), { recursive: true });
    fs.mkdirSync(path.join(platDir, 'bin'), { recursive: true });
    for (const f of ['platform.js', 'postinstall.js', 'bin/gh-pr-manager']) {
      fs.copyFileSync(path.join(__dirname, f), path.join(launcherDir, f));
    }
    fs.writeFileSync(path.join(launcherDir, 'package.json'), JSON.stringify({ name: platform.LAUNCHER }));
    fs.writeFileSync(path.join(platDir, 'package.json'), JSON.stringify({ name: wanted.pkg }));
    const fake = path.join(platDir, 'bin', wanted.exe);
    fs.writeFileSync(fake, '#!/bin/sh\necho "gh-pr-manager 0.1.2 (deadbee, 2026-10-18) $*"\nexit 3\n');
    fs.chmodSync(fake, 0o755);

    const run = spawnSync('node', [path.join(launcherDir, 'bin/gh-pr-manager'), '--version'], { encoding: 'utf8' });
    assert.equal(run.status, 3, run.stderr);
    assert.match(run.stdout, /gh-pr-manager 0\.1\.2 \(deadbee, 2026-10-18\) --version/);

    const post = spawnSync('node', [path.join(launcherDir, 'postinstall.js')], { encoding: 'utf8' });
    assert.equal(post.status, 0, post.stderr + post.stdout);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
