# gh-pr-manager

A mouse-friendly terminal UI for browsing and managing GitHub pull requests.

```sh
npx gh-pr-manager@latest          # always the newest release
npx gh-pr-manager@0.2.0           # or pin a version
npx gh-pr-manager@latest --help
```

Auth: `GH_TOKEN` / `GITHUB_TOKEN`, otherwise the GitHub CLI's token
(`gh auth login`).

The binary it runs is the one attached to the GitHub release of the same
version; `--version` answers with that version, commit and date.

## Platforms

- Linux x86_64 and arm64 (glibc 2.35 or newer)
- macOS Apple Silicon and Intel
- Windows x64

This launcher is a thin package. Each platform is an optional dependency
npm installs only when `os` / `cpu` / `libc` match, so a Mac never
downloads the Linux binary.

## Source

<https://github.com/stealth-factory/gh-pr-manager>

## License

[GNU AGPL-3.0](LICENSE).
