// gh-pr-manager - a terminal UI for GitHub pull requests
// Copyright (C) 2026 William Li
// SPDX-License-Identifier: AGPL-3.0-or-later

'use strict';

// Refuses a publish if `latest` already names a newer version on any
// of the published packages. tag-release.yml dispatches release.yml without
// waiting, so two tagged runs can overlap their builds; the publish
// job's concurrency group lets only one write at a time, but an older
// run queued behind a newer one would otherwise move latest backwards
// the moment it writes.
//
// Unpublished is fine. A latest that is not x.y.z is not — fail
// closed rather than guess.

const { execFileSync } = require('child_process');
const platform = require('./platform');

function packageNames() {
  return [platform.LAUNCHER, ...platform.PLATFORMS.map((p) => p.pkg)];
}

function parse(v) {
  const m = String(v).trim().match(/^(\d+)\.(\d+)\.(\d+)$/);
  if (!m) return null;
  return [Number(m[1]), Number(m[2]), Number(m[3])];
}

function cmp(a, b) {
  for (let i = 0; i < 3; i++) {
    if (a[i] !== b[i]) return a[i] - b[i];
  }
  return 0;
}

function viewLatest(name) {
  try {
    return execFileSync('npm', ['view', name, 'version'], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
    }).trim();
  } catch {
    return null;
  }
}

function refuseStaleLatest({ version, names, view }) {
  const mine = parse(version);
  if (!mine) {
    return `ref version is not x.y.z: ${version}`;
  }
  for (const name of names) {
    const raw = view(name);
    if (raw == null || raw === '') continue;
    const theirs = parse(raw);
    if (!theirs) {
      return `${name}@latest is not x.y.z: ${raw}`;
    }
    if (cmp(theirs, mine) > 0) {
      return (
        `${name}@latest is ${raw}; this run is ${version}. ` +
        'A newer publish already owns latest. Cut the next version.'
      );
    }
  }
  return null;
}

function main(argv, view = viewLatest) {
  const version = argv[2];
  if (!version) {
    console.error('usage: refuse-stale-latest.js <version>');
    return 2;
  }
  const reason = refuseStaleLatest({
    version,
    names: packageNames(),
    view,
  });
  if (reason) {
    console.error(reason);
    return 1;
  }
  return 0;
}

if (require.main === module) {
  process.exit(main(process.argv));
}

module.exports = {
  parse,
  cmp,
  packageNames,
  refuseStaleLatest,
  main,
};
