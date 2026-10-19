'use strict';
// Run with `node --test debian/test.js`. CI runs it on every pull request.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { stage, NAME } = require('./package');

function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'gh-pr-manager-deb-test-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  fs.mkdirSync(path.join(root, 'bin'));
  for (const name of ['README.md', 'LICENSE', 'CHANGELOG.md']) fs.writeFileSync(path.join(root, name), 'fixture');
  const elf = Buffer.alloc(64); elf.write('7f454c460201', 'hex'); elf.writeUInt16LE(62, 18);
  fs.writeFileSync(path.join(root, 'bin', NAME), elf, { mode: 0o755 });
  return root;
}

test('the binary lands in /usr/bin with docs and amd64 metadata', t => {
  const root = fixture(t);
  const out = path.join(root, 'out');
  stage('1.2.3', path.join(root, 'bin'), out, root);
  assert.deepEqual(fs.readdirSync(path.join(out, 'usr/bin')), [NAME]);
  assert.deepEqual(fs.readdirSync(path.join(out, `usr/share/doc/${NAME}`)).sort(), ['CHANGELOG.md', 'README.md', 'copyright']);
  const control = fs.readFileSync(path.join(out, 'DEBIAN/control'), 'utf8');
  assert.match(control, /^Package: gh-pr-manager\nVersion: 1\.2\.3\n/);
  assert.match(control, /Architecture: amd64\n[\s\S]*Depends: libc6 \(>= 2.35\), libgcc-s1/);
  assert.throws(() => stage('1.2.3', path.join(root, 'bin'), out, root), /EEXIST/);
});

test('reject missing, symlinked and foreign binaries and invalid versions', t => {
  const root = fixture(t); const bin = path.join(root, 'bin'); const out = path.join(root, 'out');
  for (const v of ['dev', 'v1.2.3', '1.2.3\n', '1.2.3-rc.1']) assert.throws(() => stage(v, bin, out, root), /stable release/);
  const file = path.join(bin, NAME);
  const arm = Buffer.alloc(64); arm.write('7f454c460201', 'hex'); arm.writeUInt16LE(183, 18);
  fs.writeFileSync(file, arm, { mode: 0o755 });
  assert.throws(() => stage('1.2.3', bin, out, root), /ELF64/);
  fs.writeFileSync(file, '#!/bin/sh\n', { mode: 0o755 });
  assert.throws(() => stage('1.2.3', bin, out, root), /ELF64/);
  fs.unlinkSync(file);
  assert.throws(() => stage('1.2.3', bin, out, root), /ENOENT/);
  fs.symlinkSync('/bin/sh', file);
  assert.throws(() => stage('1.2.3', bin, out, root), /ELF64/);
});

test('APT notification carries only the exact allowlisted release, never credentials', () => {
  const { payload, notify } = require('./notify-apt');
  const env = { GITHUB_REPOSITORY: 'stealth-factory/gh-pr-manager', GITHUB_REF_NAME: 'v1.2.3', GITHUB_SHA: 'a'.repeat(40), GH_TOKEN: 'non-secret-fixture' };
  let call;
  notify(env, (...args) => { call = args; });
  assert.deepEqual(JSON.parse(call[2].input), payload(env.GITHUB_REPOSITORY, env.GITHUB_REF_NAME, env.GITHUB_SHA));
  assert.equal(call[1][3], 'repos/stealth-factory/apt-repo/dispatches');
  assert.equal(call[2].input.includes(env.GH_TOKEN), false);
  for (const tag of ['dev', '1.2.3', 'v1.2.3-rc.1', 'v1.2.3\n']) assert.throws(() => payload(env.GITHUB_REPOSITORY, tag, env.GITHUB_SHA));
  assert.throws(() => payload('another/repo', 'v1.2.3', env.GITHUB_SHA));
  assert.throws(() => payload(env.GITHUB_REPOSITORY, 'v1.2.3', 'main'));
  assert.throws(() => notify({ ...env, GH_TOKEN: '' }), /unavailable/);
});
