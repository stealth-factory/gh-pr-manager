// gh-pr-manager - a terminal UI for GitHub pull requests
// Copyright (C) 2026 William Li
// SPDX-License-Identifier: AGPL-3.0-or-later

'use strict';

// Ported from opscope's npm/platform.js. The launcher package is a thin
// Node shim; each platform is an optionalDependency npm installs only when
// its `os` / `cpu` / `libc` selectors match, so a Mac never downloads the
// Linux binary.

const fs = require('fs');
const path = require('path');

// The unscoped name is free on npm (checked 2026-10-18), which is why this
// is `gh-pr-manager` and not a scoped package: `npx gh-pr-manager@latest`
// is the command somebody with only Node is asked to type.
const LAUNCHER = 'gh-pr-manager';

// One row per artefact release.yml actually produces. A platform that is
// only a wish is not a row: anything absent here fails at install with a
// sentence saying what is published. `rust` is the release tarball suffix,
// and is how pack.js finds the right artefact. `exe` is the binary's file
// name inside the tarball and the platform package.
const PLATFORMS = [
  {
    pkg: 'gh-pr-manager-linux-x64',
    os: 'linux',
    cpu: 'x64',
    libc: 'glibc',
    rust: 'x86_64-unknown-linux-gnu',
    exe: 'gh-pr-manager',
    label: 'Linux x86_64 (glibc)',
  },
  {
    pkg: 'gh-pr-manager-linux-arm64',
    os: 'linux',
    cpu: 'arm64',
    libc: 'glibc',
    rust: 'aarch64-unknown-linux-gnu',
    exe: 'gh-pr-manager',
    label: 'Linux arm64 (glibc)',
  },
  {
    pkg: 'gh-pr-manager-darwin-arm64',
    os: 'darwin',
    cpu: 'arm64',
    rust: 'aarch64-apple-darwin',
    exe: 'gh-pr-manager',
    label: 'macOS Apple Silicon',
  },
  {
    pkg: 'gh-pr-manager-darwin-x64',
    os: 'darwin',
    cpu: 'x64',
    rust: 'x86_64-apple-darwin',
    exe: 'gh-pr-manager',
    label: 'macOS Intel',
  },
  {
    pkg: 'gh-pr-manager-win32-x64',
    os: 'win32',
    cpu: 'x64',
    rust: 'x86_64-pc-windows-msvc',
    exe: 'gh-pr-manager.exe',
    label: 'Windows x64',
  },
];

// Both Linux binaries are built on Ubuntu 22.04 (x64 and arm64 runners),
// which is glibc 2.35 - the oldest glibc they will load on. npm's `libc`
// selector cannot see the version, so it is compared here and an older
// machine fails at install the same way musl does.
const MIN_GLIBC = '2.35';

function parseGlibc(version) {
  const m = String(version).trim().match(/^(\d+)\.(\d+)/);
  if (!m) return null;
  return [Number(m[1]), Number(m[2])];
}

function glibcAtLeast(have, need) {
  const a = parseGlibc(have);
  const b = parseGlibc(need);
  if (!a || !b) return false;
  return a[0] > b[0] || (a[0] === b[0] && a[1] >= b[1]);
}

function glibcTooOld(h) {
  return Boolean(h && h.os === 'linux' && h.glibc && !glibcAtLeast(h.glibc, MIN_GLIBC));
}

function linuxRuntime() {
  let glibc;
  try {
    const report = process.report && process.report.getReport && process.report.getReport();
    if (report && report.header && report.header.glibcVersionRuntime) {
      glibc = report.header.glibcVersionRuntime;
    }
  } catch (_) {
    // getReport can throw in some embeddings. Fall through.
  }
  if (glibc) return { libc: 'glibc', glibc };
  try {
    if (
      fs.existsSync('/lib/ld-musl-x86_64.so.1') ||
      fs.existsSync('/lib/ld-musl-aarch64.so.1')
    ) {
      return { libc: 'musl' };
    }
  } catch (_) {
    // A filesystem we cannot read is not evidence of musl.
  }
  // Linux and nothing said musl: refuse only when we can read the glibc
  // version and it is old, never on a missing reading.
  return { libc: 'glibc' };
}

function host() {
  if (process.platform !== 'linux') {
    return { os: process.platform, cpu: process.arch };
  }
  const linux = linuxRuntime();
  return { os: process.platform, cpu: process.arch, libc: linux.libc, glibc: linux.glibc };
}

function describeHost(h) {
  const who = h || host();
  if (glibcTooOld(who)) return `${who.os}-${who.cpu} (glibc ${who.glibc})`;
  const libc = who.libc ? ` (${who.libc})` : '';
  return `${who.os}-${who.cpu}${libc}`;
}

function currentPlatform(h) {
  const who = h || host();
  if (glibcTooOld(who)) return null;
  return (
    PLATFORMS.find(
      (p) => p.os === who.os && p.cpu === who.cpu && (p.libc === undefined || p.libc === who.libc),
    ) || null
  );
}

function publishedPlatforms() {
  return PLATFORMS.map((p) => `  ${p.label}`).join('\n');
}

const SOURCE = 'Build from source: https://github.com/stealth-factory/gh-pr-manager';

function unsupportedMessage(h) {
  const who = h || host();
  if (glibcTooOld(who)) {
    return [
      `${LAUNCHER} needs glibc ${MIN_GLIBC} or newer; this machine has ${who.glibc}.`,
      '',
      `The Linux binaries are built against glibc ${MIN_GLIBC}. Build from source`,
      'on this machine, or run it where glibc is new enough.',
      '',
      'It publishes:',
      publishedPlatforms(),
      '',
      SOURCE,
    ].join('\n');
  }
  return [
    `${LAUNCHER} has no binary for ${describeHost(who)}.`,
    '',
    'It publishes:',
    publishedPlatforms(),
    '',
    SOURCE,
  ].join('\n');
}

function missingOptionalMessage(wanted) {
  return [
    `this machine is ${wanted.label}, which is published, but`,
    `${wanted.pkg} did not install.`,
    '',
    'npm skipped it - --omit=optional, an offline registry, or a',
    'network error. Retry without omitting optional dependencies,',
    `or install ${wanted.pkg} at this same version.`,
  ].join('\n');
}

function installedPackageDir(pkg) {
  try {
    return path.dirname(require.resolve(`${pkg}/package.json`));
  } catch (_) {
    return null;
  }
}

function resolveStart(h) {
  const wanted = currentPlatform(h);
  if (!wanted) {
    const err = new Error(unsupportedMessage(h));
    err.code = 'UNSUPPORTED_PLATFORM';
    throw err;
  }
  const dir = installedPackageDir(wanted.pkg);
  if (!dir) {
    const err = new Error(missingOptionalMessage(wanted));
    err.code = 'MISSING_OPTIONAL';
    throw err;
  }
  const start = path.join(dir, 'bin', wanted.exe);
  if (!fs.existsSync(start)) {
    const err = new Error(`${wanted.pkg} is installed but bin/${wanted.exe} is missing. Reinstall the package.`);
    err.code = 'CORRUPT_OPTIONAL';
    throw err;
  }
  return start;
}

function requireInstalled(h) {
  resolveStart(h);
}

module.exports = {
  LAUNCHER,
  PLATFORMS,
  MIN_GLIBC,
  glibcAtLeast,
  host,
  describeHost,
  currentPlatform,
  publishedPlatforms,
  unsupportedMessage,
  missingOptionalMessage,
  installedPackageDir,
  resolveStart,
  requireInstalled,
};
