# gh-pr-manager

[![npm](https://img.shields.io/npm/v/gh-pr-manager)](https://www.npmjs.com/package/gh-pr-manager)
[![ci](https://github.com/stealth-factory/gh-pr-manager/actions/workflows/ci.yml/badge.svg)](https://github.com/stealth-factory/gh-pr-manager/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/stealth-factory/gh-pr-manager)](https://github.com/stealth-factory/gh-pr-manager/releases/latest)

A terminal UI for keeping up with your GitHub pull requests: browse them by scope, read details, CI and the full timeline, comment (with `@mention` autocomplete for people and bots), approve, merge / squash / rebase, or close — with full mouse support.

## Run it

**npm is the default and recommended method:**

```sh
npx gh-pr-manager@latest
```

Nothing to install and no `PATH` to manage: it fetches the prebuilt binary for this machine into npm's cache and starts it. Pin a version with `npx gh-pr-manager@0.1.0`. Published for Linux x86_64 and arm64 (glibc 2.35 or newer), macOS Apple Silicon and Intel, and Windows x64. [Other ways to get it →](#other-ways-to-get-it)

Auth: `GH_TOKEN` / `GITHUB_TOKEN`, otherwise the GitHub CLI's token (`gh auth login`). Listing your orgs needs the `read:org` scope.

## Using it

- **List screen** — every PR in the current scope. Click the `▾ scope` chip (or press `s`) to pick a scope from a centred modal; type to filter it. Click `◐ Open` (or `f`) to cycle Open → Closed → Merged → All. `/` filters the loaded list.
- **Detail screen** — `Enter` or double-click a row. Left third is the PR list, right two-thirds the details. Click a pane or press `Tab` to switch focus; the wheel scrolls whatever is under the pointer. `J`/`K` jump between PRs while reading.
- **Actions** — the button bar at the top of the detail pane, the footer chips, or keys: `c` comment, `m` merge menu (only the methods the repo allows), `a` approve, `R` request changes, `x` close/reopen, `Space` all actions, `o` open in browser, `y` copy `gh pr checkout …`. Merges are sent with `expectedHeadOid`, so they fail safely if new commits land after you looked.
- **Composer** — `Enter` is a newline, `Ctrl+S` / `Alt+Enter` sends, `Esc` closes and keeps the draft. Typing `@` pops up suggestions (configured bots, comment authors, participants, repo members); pick with `↑↓`+`Tab`/`Enter` or click.

Press `?` in the app for the full key list.

## Config

Optional, at `~/.config/gh-pr-manager/config.toml`:

```toml
refresh_secs = 60                  # PR list refresh; 0 disables
detail_refresh_secs = 20           # open PR refresh; 0 disables
default_scope = "org: acme"        # scope name as shown in the picker
bots = ["claude", "copilot", "coderabbitai", "codex"]
images = "auto"                    # inline images: auto, kitty, sixel, iterm2, halfblocks, off
image_max_rows = 20                # tallest an inline image may be (GH_PR_MANAGER_IMAGES overrides images)

[[scopes]]
name = "Platform team"
query = "org:acme label:platform"

[[scopes]]
name = "Web repo"
query = "repo:acme/web"
```

See `SPEC.md` for the full design spec.

## Other ways to get it

npm / npx remains the recommended method; these are for machines without Node.

### Homebrew (macOS and Linux)

```sh
brew install stealth-factory/tap/gh-pr-manager
```

Upgrade with `brew update && brew upgrade gh-pr-manager`.

### Debian / Ubuntu (amd64)

Download `gh-pr-manager_X.Y.Z_amd64.deb` and its `.sha256` from the [latest release](https://github.com/stealth-factory/gh-pr-manager/releases/latest), then:

```sh
sha256sum -c gh-pr-manager_X.Y.Z_amd64.deb.sha256
sudo apt install ./gh-pr-manager_X.Y.Z_amd64.deb
```

### Download a release

Every release has a `.tar.gz` per platform, each with a `.sha256` beside it. The subshell lets a failed step stop the rest without closing your terminal:

```sh
( set -e
R=https://github.com/stealth-factory/gh-pr-manager/releases
V=$(curl -fsSLI -o /dev/null -w '%{url_effective}' $R/latest | sed 's|.*/||')
case "$(uname -s) $(uname -m)" in
  "Darwin arm64")   A=aarch64-apple-darwin ;;
  "Darwin x86_64")  A=x86_64-apple-darwin ;;
  "Linux x86_64")   A=x86_64-unknown-linux-gnu ;;
  "Linux aarch64")  A=aarch64-unknown-linux-gnu ;;
  *) echo "no build for $(uname -sm); build from source below" >&2; exit 1 ;;
esac
T=gh-pr-manager-$V-$A
curl -fsSLO $R/download/$V/$T.tar.gz
curl -fsSLO $R/download/$V/$T.tar.gz.sha256
shasum -a 256 -c $T.tar.gz.sha256    # sha256sum -c on Linux; either is fine
tar -xzf $T.tar.gz && ./$T/gh-pr-manager
)
```

On Windows, take `gh-pr-manager-vX.Y.Z-x86_64-pc-windows-msvc.tar.gz` from the release page and extract it with `tar -xzf`. On macOS, download with `curl` rather than a browser: the binaries are not notarised, and a browser's quarantine mark makes Gatekeeper refuse them (`xattr -dr com.apple.quarantine <dir>` clears it).

### Build from source

```sh
cargo install --git https://github.com/stealth-factory/gh-pr-manager
# or, from a checkout:
cargo build --release && ./target/release/gh-pr-manager
```

## Releasing

Releases are cut by merging the standing `release: vX.Y.Z` pull request; the version and changelog come from conventional-commit PR titles. See [docs/releasing.md](docs/releasing.md) and [CHANGELOG.md](CHANGELOG.md).

## License

[GNU AGPL-3.0](LICENSE).
