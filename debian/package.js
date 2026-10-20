'use strict';

// Builds gh-pr-manager_X.Y.Z_amd64.deb from the tagged Linux x86_64 build.
// Ported from opscope's debian/package.js. One binary, so it goes straight
// into /usr/bin; opscope's libexec layout exists to keep sixteen sibling
// widgets off PATH, which does not apply here.

const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');
const { MIN_GLIBC } = require('../npm/platform');
const { stableVersion } = require('../homebrew/formula');

const NAME = 'gh-pr-manager';

function stage(version, binaries, destination, repo = path.resolve(__dirname, '..')) {
  stableVersion(version);
  // Reject foreign architectures, scripts and symlinks before creating a
  // package whose metadata promises Linux amd64.
  const file = path.join(binaries, NAME);
  const stat = fs.lstatSync(file);
  const bytes = fs.readFileSync(file);
  if (!stat.isFile() || !(stat.mode & 0o111) || bytes.length < 20 ||
      bytes.subarray(0, 4).toString('hex') !== '7f454c46' ||
      bytes[4] !== 2 || bytes[5] !== 1 || bytes.readUInt16LE(18) !== 62) {
    throw new Error(`${NAME} must be an executable ELF64 little-endian amd64 file`);
  }
  fs.mkdirSync(destination); // Never reuse a tree with stale files.
  const put = (name, contents, mode = 0o644) => {
    const target = path.join(destination, name);
    fs.mkdirSync(path.dirname(target), { recursive: true, mode: 0o755 });
    fs.writeFileSync(target, contents, { mode });
  };
  put(`usr/bin/${NAME}`, bytes, 0o755);
  for (const name of ['README.md', 'CHANGELOG.md']) {
    put(`usr/share/doc/${NAME}/${name}`, fs.readFileSync(path.join(repo, name)));
  }
  put(`usr/share/doc/${NAME}/copyright`, fs.readFileSync(path.join(repo, 'LICENSE')));
  const files = [];
  (function walk(dir) {
    for (const entry of fs.readdirSync(path.join(destination, dir), { withFileTypes: true })) {
      const name = path.posix.join(dir, entry.name);
      if (entry.isDirectory()) walk(name); else files.push(name);
    }
  })('usr');
  const size = Math.ceil(files.reduce((n, f) => n + fs.statSync(path.join(destination, f)).size, 0) / 1024);
  put('DEBIAN/control', `Package: ${NAME}\nVersion: ${version}\nSection: devel\nPriority: optional\nArchitecture: amd64\nMaintainer: William Li <email@wiiiimm.codes>\nDepends: libc6 (>= ${MIN_GLIBC}), libgcc-s1\nInstalled-Size: ${size}\nHomepage: https://github.com/stealth-factory/gh-pr-manager\nDescription: terminal UI for browsing and managing GitHub pull requests\n Browse pull requests by scope, read details, CI and the timeline,\n comment, approve, merge or close, with full mouse support.\n No Node or Rust runtime is required.\n`);
  put('DEBIAN/md5sums', files.sort().map(f =>
    `${crypto.createHash('md5').update(fs.readFileSync(path.join(destination, f))).digest('hex')}  ${f}\n`).join(''));
}

function build(version, binaries, output) {
  stableVersion(version);
  if (process.platform !== 'linux') throw new Error('Build Debian packages on Linux');
  const root = path.resolve(__dirname, '..');
  const manifest = fs.readFileSync(path.join(root, 'Cargo.toml'), 'utf8').match(/^version = "([^"]+)"/m)[1];
  if (version !== manifest) throw new Error('Package version must match Cargo.toml');
  const reported = execFileSync(path.join(path.resolve(binaries), NAME), ['--version'], { encoding: 'utf8' }).trim();
  if (!new RegExp(`^${NAME} ${version.replace(/\./g, '\\.')} \\([0-9a-f]{7,}, \\d{4}-\\d{2}-\\d{2}\\)$`).test(reported)) {
    throw new Error(`Inconsistent release stamp: ${reported}`);
  }
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), `${NAME}-deb-`));
  try {
    const tree = path.join(temp, 'root');
    stage(version, binaries, tree);
    const epoch = execFileSync('git', ['show', '-s', '--format=%ct', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim();
    fs.mkdirSync(output, { recursive: true });
    const name = `${NAME}_${version}_amd64.deb`;
    const file = path.resolve(output, name);
    if (fs.existsSync(file)) throw new Error(`Refusing to overwrite ${file}`);
    execFileSync('dpkg-deb', ['--root-owner-group', '-Zxz', '--build', tree, file], {
      stdio: 'inherit', env: { ...process.env, SOURCE_DATE_EPOCH: epoch },
    });
    const hash = crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
    fs.writeFileSync(`${file}.sha256`, `${hash}  ${name}\n`);
  } finally { fs.rmSync(temp, { recursive: true, force: true }); }
}

if (require.main === module) {
  if (process.argv.length !== 5) throw new Error('Usage: node debian/package.js X.Y.Z BINARY_DIR OUTPUT_DIR');
  build(...process.argv.slice(2));
}
module.exports = { stage, build, NAME };
