# Changelog

Every release, and what changed in it. Generated from the commit subjects,
which is why they are worth writing carefully.

## [0.2.0] - 2026-11-04

### CI

- **release**: Mint the org GitHub App token instead of the bot user PAT (#8)
- **release**: Rewrite the pooled release pull request body (#9)

### Features

- Universal search palette with live github and pr number lookup (#2)
- Unread markers and refresh digest for changed prs (#3)
- Tab badges, new markers, notifications and read sync for changes (#4)

## [0.1.0] - 2026-10-18

The first release.

### Features

- Browse pull requests by scope (your PRs, review requests, orgs, repos,
  custom search queries), filtered by state and text
- A detail view with the description, CI checks and the full timeline
- Comment with `@mention` autocomplete for people and configured bots,
  approve, request changes, merge / squash / rebase, close and reopen
- Full mouse support: clickable chips, buttons and panes, wheel scrolling
- Authentication from `GH_TOKEN` / `GITHUB_TOKEN` or the GitHub CLI
- Optional configuration at `~/.config/gh-pr-manager/config.toml`
