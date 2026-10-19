# Releasing

A release is one decision: **merge the release pull request.** Everything
either side of it is automatic. This is the same pipeline opscope uses,
adapted to one binary and five platforms.

## What happens, in order

```
merge a PR to main
        │
        ├─► ci.yml            tests and builds it (Linux, macOS, Windows,
        │                     plus a .deb install/upgrade/remove check)
        │
        └─► release-pr.yml    works out the next version from the commit
                              subjects, writes the changelog entry, bumps
                              Cargo.toml and Cargo.lock, and force-pushes
                              the `release` branch - opening or updating
                              one standing PR titled `release: vX.Y.Z`

           ... this repeats for every merge. Nothing is tagged or
               published. The PR accumulates until you decide.

merge the release PR
        │
        └─► tag-release.yml   sees Cargo.toml names a version with no tag,
                              tags the merge commit, and starts release.yml
                              at that tag
                    │
                    └─► release.yml   five targets built and tested,
                                      checked for dynamic libraries beyond
                                      the system runtime, tarballed with
                                      checksums, a .deb built and taken
                                      through install/upgrade/remove,
                                      packed into six npm packages from
                                      those same tarballs, published to
                                      npm - which moves `latest` - then
                                      attached to a GitHub Release
                              │
                              ├─► homebrew        formula generated from
                              │                   the release, installed and
                              │                   tested, written to
                              │                   stealth-factory/homebrew-tap
                              ├─► apt-repository  notifies the shared APT repo
                              └─► smoke-npm       clean Linux x64/arm64,
                                                  macOS and Windows runners
                                                  run `npx gh-pr-manager@X.Y.Z
                                                  --version`

           smoke-npm is not a gate. It runs after the release is out, so it
           reports on it rather than deciding it; a red one is a release to
           fix forward, not a release held back.
```

## What decides the version

The commit subjects since the last tag, read by `git-cliff` under the rules
in `cliff.toml`. Squash merge means **the pull request title is the commit
subject**, so in practice the version is decided by PR titles.

| Title starts with | Effect |
|---|---|
| `feat:` / `feat(scope):` | minor - `0.1.0` → `0.2.0` |
| `fix:`, `perf:`, `revert:` | patch - `0.1.0` → `0.1.1` |
| any type with `!:` | breaking - minor below 1.0.0 (semver's rule for 0.x) |
| `docs:`, `chore:`, `ci:`, `style:`, `test:`, `refactor:`, `build:` | rides along with the next release; starts none by itself |
| `release:` | skipped entirely - the machinery's own commits |

A batch of documentation and chores raises no release PR: a version whose
binary is byte-for-byte the previous one's is noise. Those commits still
appear in the changelog of whatever release comes next.

`pr-title.yml` enforces the format on every pull request: lowercase subject,
no trailing full stop. A badly-titled PR is a change the next release will
not mention and may not count towards the version.

## The version lives in one place

`Cargo.toml`'s `version`. Never bump it by hand. Everything else takes its
version from the tag that release-pr.yml's bump leads to:

- the binary's `--version` (`gh-pr-manager X.Y.Z (<commit>, <date>)`,
  stamped by `build.rs`);
- the tarball and .deb names;
- every npm package (`npm/pack.js` stamps the tag onto all six and refuses
  one that disagrees with Cargo.toml; `npm/package.json` stays at
  `0.0.0-dev` in git);
- the Homebrew formula.

## Cutting one

1. Open the pull request titled `release: vX.Y.Z`.
2. Read the changelog entry. If something reads badly, fix it on main - do
   not commit to the `release` branch, which is force-pushed.
3. Merge it.
4. Watch `release.yml`.

If no release PR exists, nothing releasable has landed since the last tag.
The `release-pr` run log says which, with the count it found.

## The first release (v0.1.0)

There is no tag yet and Cargo.toml says `0.1.0`, so the first push to main
after this pipeline lands makes `tag-release.yml` tag `v0.1.0` and start
`release.yml`. `CHANGELOG.md` carries a hand-written `0.1.0` section for
it. That tag is also the baseline later changelogs count from.

npm trusted publishing is configured per package, and the packages have to
exist first. So before that push, or as a recovery right after it:

1. Let `release.yml` build v0.1.0. Its publish step fails on the first
   package with nothing published, and no GitHub release is created.
2. Download the run's artefacts, check out the tag (`git checkout v0.1.0`),
   and in that checkout run
   `node npm/pack.js --version 0.1.0 --artifacts <downloaded dir> --out npm-dist`.
3. `npm login`, then publish the five platform packages and then the
   launcher: `npm publish ./npm-dist/gh-pr-manager-linux-x64 --access public`
   and so on, ending with `npm publish ./npm-dist/gh-pr-manager --access public`.
   Publishing from the tagged checkout records that commit as `gitHead`.
4. On npmjs.com, add a trusted publisher to each of the six packages:
   GitHub Actions, `stealth-factory/gh-pr-manager`, workflow `release.yml`.
5. Re-run `release.yml` at the tag (`gh workflow run release.yml --ref v0.1.0`).
   Publish sees each version already on npm from this commit, skips it, and
   goes on to create the GitHub release, the formula and the smoke tests.

From then on every release publishes with no token at all.

## Things worth knowing

**The manifest moves ahead of the tag, deliberately.** The release PR bumps
`Cargo.toml` so the tag points at a commit whose version already matches it.
`release.yml` checks the two agree and refuses to build otherwise.

**The release PR gets ordinary CI when it is opened by a user token.**
`release-pr.yml` uses `GH_TOKEN` (or `RELEASE_TOKEN`) for the checkout and
to open the PR. A classic PAT with `repo` is enough; a fine-grained one
needs Contents and Pull requests read/write. Opened with `GITHUB_TOKEN`
instead, the PR is `github-actions[bot]` and GitHub holds `ci` for a
maintainer click. It still runs `cargo metadata --locked` on the bumped tree
before offering it.

**`GH_TOKEN` / `RELEASE_TOKEN` are optional for releasing, required for the
tap and APT.** A tag pushed with `GITHUB_TOKEN` triggers no workflow, so
`tag-release.yml` starts the build with an explicit `workflow_dispatch` at
the tag rather than relying on a PAT. The Homebrew tap update and the APT
notification do need that token, with Contents write on
`stealth-factory/homebrew-tap` and `stealth-factory/apt-repo`.

**npm publish uses trusted publishing, and nothing else.** No token,
`id-token: write`, published with provenance. Granular npm tokens expire
after 90 days, which is a release-day failure waiting in the calendar. The
publish step deletes the `_authToken` line `setup-node` writes, because any
credential at all makes npm skip the OIDC exchange.

**Publishing moves `latest`, and there is a propagation window.** npm can
take a few minutes to serve a version after accepting it; for those minutes
`npx gh-pr-manager@latest` resolves a version it cannot fetch. opscope tried
publishing under `next` and promoting after the smoke test, but
`npm dist-tag add` cannot use OIDC ([npm/cli#8547](https://github.com/npm/cli/issues/8547)).
When that lands, the split can come back. `smoke-npm` retries for eight
minutes.

Publish jobs for different tags share one concurrency group, and
`npm/refuse-stale-latest.js` refuses to publish if `latest` already names a
newer version, so an older queued run cannot roll `latest` backwards.

**Both macOS architectures are built on one Apple Silicon runner.** The
x86_64 leg is cross-compiled, so it is built and packaged but its tests and
`--version` check are not run; the same sources are tested on arm64. Linux
arm64 uses GitHub's native `ubuntu-22.04-arm` runner, so it is fully tested.

**Windows links the C runtime statically** (`.cargo/config.toml`), so the
binary starts without the Visual C++ Redistributable; `release.yml` checks
it imports no `vcruntime`/`api-ms-win-crt` DLL.

## If something goes wrong

**A tag exists but has no release.** The build failed after tagging. Read
`release.yml`'s log. If npm published nothing, delete the tag and let
`tag-release.yml` re-create it after the fix is on main (re-dispatching the
same tag builds that tag's tree, so a fix only on main will not run). If any
package already landed on npm, that version is frozen - cut the next one.

**npm publish failed and there is no GitHub release.** Check all six
packages on npm (`gh-pr-manager` and the five `gh-pr-manager-<os>-<cpu>`)
before retrying - the publishes are not atomic. If none have this version,
trusted publishing is likely not configured: fix that, then re-dispatch
`release.yml` at the tag. A version already on npm from this same commit
(its `gitHead` matches) is skipped, so a retry finishes the release. One
from a different commit, or with no readable `gitHead`, is an error.

**`smoke-npm` went red.** `latest` already names this version and a clean
runner could not install it. Cut the next version; do not unpublish.

**The release PR is not updating.** Check `release-pr.yml`'s last run. It
exits quietly when nothing releasable has landed, which looks identical to
being broken; the log says which.

**A version was cut by mistake.** Do not delete the release - cut the next
one.
