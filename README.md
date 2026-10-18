# gh-pr-manager

A terminal UI for keeping up with your GitHub pull requests: browse them by scope, read details, CI and the full timeline, comment (with `@mention` autocomplete for people and bots), approve, merge / squash / rebase, or close — with full mouse support.

```sh
cargo run --release
```

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
refresh_secs = 120                 # 0 disables auto-refresh
default_scope = "org: acme"        # scope name as shown in the picker
bots = ["claude", "copilot", "coderabbitai", "codex"]

[[scopes]]
name = "Platform team"
query = "org:acme label:platform"

[[scopes]]
name = "Web repo"
query = "repo:acme/web"
```

See `SPEC.md` for the full design spec.
