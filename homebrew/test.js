'use strict';

// Run with `node --test homebrew/test.js`. CI runs it on every pull request.

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { spawnSync } = require('node:child_process');
const { generateFormula, stableVersion, newer, TARGETS, NAME } = require('./formula');

function release(t, version, { skipBinary, badSum } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'gh-pr-manager-brew-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  for (const target of Object.values(TARGETS)) {
    const root = `${NAME}-v${version}-${target}`;
    fs.mkdirSync(path.join(dir, root));
    if (!skipBinary) fs.writeFileSync(path.join(dir, root, NAME), 'binary');
    fs.writeFileSync(path.join(dir, root, 'README.md'), 'readme');
    const tar = spawnSync('tar', ['-czf', `${root}.tar.gz`, root], { cwd: dir });
    assert.equal(tar.status, 0);
    const hash = badSum ? '0'.repeat(64)
      : crypto.createHash('sha256').update(fs.readFileSync(path.join(dir, `${root}.tar.gz`))).digest('hex');
    fs.writeFileSync(path.join(dir, `${root}.tar.gz.sha256`), `${hash}  ${root}.tar.gz\n`);
  }
  return dir;
}

test('the formula names every platform archive with its checksum', t => {
  const dir = release(t, '1.2.3');
  const formula = generateFormula('1.2.3', dir);
  assert.match(formula, /^# Release: 1\.2\.3$/m);
  assert.match(formula, /class GhPrManager < Formula/);
  for (const target of Object.values(TARGETS)) {
    assert.ok(formula.includes(`/v1.2.3/${NAME}-v1.2.3-${target}.tar.gz`), target);
  }
  assert.equal((formula.match(/sha256 "[0-9a-f]{64}"/g) || []).length, 4);
});

test('a bad checksum or a missing binary is refused', t => {
  assert.throws(() => generateFormula('1.2.3', release(t, '1.2.3', { badSum: true })), /Checksum mismatch/);
  assert.throws(() => generateFormula('1.2.3', release(t, '1.2.3', { skipBinary: true })), /missing binary/);
});

test('only stable versions, compared numerically', () => {
  for (const v of ['dev', 'v1.2.3', '1.2', '1.2.3-rc.1', ' 1.2.3']) assert.throws(() => stableVersion(v));
  assert.equal(newer('1.10.0', '1.9.0'), true);
  assert.equal(newer('1.9.0', '1.10.0'), false);
  assert.equal(newer('1.9.0', '1.9.0'), false);
});
