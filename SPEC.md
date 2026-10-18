# gh-pr-manager — Product + Technical Spec

A Rust TUI for watching, commenting on, and merging the GitHub pull requests that matter to you.
Stack: ratatui 0.30 (re-exported crossterm 0.29 via `ratatui::crossterm`, mouse capture on), tokio,
reqwest 0.13 + serde_json against the GitHub GraphQL v4 API, optional TOML config.

Every GraphQL field in section 7 was validated read-only against the live schema with `gh api graphql`
on 2026-10-18. Notes on surprises are in section 7.6.

---

## 1. Screens and layout

### 1.1 Frame skeleton (both screens)

```
┌ header (1 row) ──────────────────────────────────────────────────────────────┐
│ Scope: My PRs ▾ │ Open · Closed · Merged · All │ 36 of 2522 ⠋ │ api 3778 │ @wiiiimm │
├ body (fills) ────────────────────────────────────────────────────────────────┤
│                                                                              │
├ status/toast (1 row) ────────────────────────────────────────────────────────┤
│ ✔ Comment posted on stealth-factory/opscope#439                              │
├ footer key hints (1 row) ────────────────────────────────────────────────────┤
│ ↑↓ move  ⏎ open  s scope  f filter  r refresh  o browser  ? help  q quit      │
└──────────────────────────────────────────────────────────────────────────────┘
```

| Region | Content | Clickable |
|---|---|---|
| Header | Scope chip (`Scope: <name> ▾`), state-filter tabs (active one bold/underlined), counts `<loaded> of <issueCount>`, braille spinner while any request is in flight, `api <remaining>` (red when < 300), viewer login | Scope chip opens scope modal; each filter tab sets the filter |
| Body | List screen or split detail screen (below) | yes |
| Status line | Latest toast: `✔` success (green, auto-clears after 5 s), `✖` error (red, persists until next action/keypress), `…` progress (`Merging #98…`). Empty otherwise | Click clears |
| Footer | Context-sensitive hints as `key label` chips; each chip's Rect is stored so a click fires that action. Overflowing chips are dropped from the right | yes |

Minimum size 70x16; below that render a single centered message "Terminal too small (70x16 min)".

### 1.2 List screen

One ratatui `Table` with `TableState` (row highlight) and a vertical `Scrollbar`.

| Column | Width | Content |
|---|---|---|
| St | 2 | `●` green open, `○` grey draft, `✔` magenta merged, `✖` red closed |
| Repo | Min 18, Max 32 | `owner/name`, dim owner |
| # | 6 | `#298` |
| Title | Fill | truncated with `…`; dim when draft |
| Author | 12 | login, `@me` highlighted |
| CI | 3 | `✓` success, `✗` failure/error, `●` pending (yellow), `–` none |
| Rev | 3 | `✓` APPROVED, `✗` CHANGES_REQUESTED, `●` REVIEW_REQUIRED, `–` null |
| Upd | 7 | relative: `5m`, `3h`, `2d`, else `Oct 3` |

- Header row shows column names; a `⠋` spinner replaces `St` header while loading.
- Rows come from the search query (section 2) 50 at a time. When the selection reaches the last loaded row and `hasNextPage`, the next page is fetched and appended (status line: `Loading more…`).
- Selection is tracked by PR node id, not index, so refreshes keep the cursor on the same PR.
- Empty state (no rows, not loading): centered text `No pull requests for "<query>"` + `Press s to change scope, f to change filter`.
- Loading state (no rows yet): centered `Loading pull requests…` with spinner.
- Error state (no rows, last fetch failed): centered red error + `r to retry`.

### 1.3 Detail screen (split)

`Layout::horizontal([Ratio(1,3), Ratio(2,3)])`. Focus is one of `Sidebar | Detail`, shown by a
cyan border on the focused pane (dim grey on the other).

- **Sidebar (left 1/3)**: same rows/selection as the list screen, compact columns `St | # | Title`
  (repo shown dim on a second line only if the pane is >= 40 cols; otherwise title only).
  Moving the selection (keys or click) loads that PR's detail after a 150 ms debounce.
- **Detail pane (right 2/3)**: scrollable document for the selected PR (section 3).

Enter on a list row opens the detail screen with `Detail` focused. Esc from the detail screen
returns to the list screen (selection preserved).

---

## 2. Scopes, state filter, and query composition

### 2.1 Scopes

| Order | Name | Qualifier | Source |
|---|---|---|---|
| 1 | My PRs | `author:@me` | built-in (default on start) |
| 2 | Review requested | `review-requested:@me` | built-in |
| 3 | Involves me | `involves:@me` | built-in |
| 4 | Assigned to me | `assignee:@me` | built-in |
| 5 | My repos (`<login>`) | `user:<login>` | built-in, from `viewer.login` |
| 6.. | Org `<org>` | `org:<org>` | one per `viewer.organizations` node |
| n.. | custom | `[[scopes]].query` from config | config |

Custom scopes are appended after built-ins; a custom scope with the same name as a built-in replaces it.

### 2.2 State filter

| Filter | Appended qualifier |
|---|---|
| Open (default) | `is:open` |
| Closed | `is:closed is:unmerged` |
| Merged | `is:merged` |
| All | (nothing) |

### 2.3 Query composition (exact)

```
format!("is:pr {scope_qualifier} {state_qualifier} archived:false sort:updated-desc")
```
with duplicate spaces collapsed. The `search` GraphQL field is called with `type: ISSUE`, `first: 50`,
`after: <cursor|null>`. Changing scope or filter bumps `list_gen`, clears rows, and refetches.

### 2.4 Scope modal

Centered popup `area.centered(Constraint::Percentage(50), Constraint::Length(min(rows+4, 20)))`,
bordered, title `Select scope`. Row 1 is a filter input (`> █`); below it one row per scope
(`name` left, dim qualifier right). Type-to-filter is case-insensitive substring on name and
qualifier. ↑/↓ move, Enter selects, Esc closes; the current scope is pre-highlighted.
Clicking a row selects it; clicking outside the popup dismisses; wheel scrolls the list.

---

## 3. Detail pane content

Rendered top-to-bottom as a `Vec<Line>` that is word-wrapped by the app itself (using
`unicode-width`) to the pane's inner width, cached per `(pr_id, width, detail_gen)`.
Scrolling is a line offset `detail_scroll` clamped to `total_lines - visible_rows`, drawn with
`Paragraph::scroll((offset, 0))` plus a `Scrollbar`. Doing our own wrapping gives an exact total
line count without relying on unstable ratatui features.

Sections, in order (each separated by a blank line; section titles bold):

1. **Title line**: `#98 feat(studio): add a project Assets tab…` bold, wrapped.
2. **Badges** (one line): state badge `OPEN`/`DRAFT`/`MERGED`/`CLOSED` (reverse video, colored as
   in 1.2), then `mergeable` + `mergeStateStatus` badge:

   | mergeStateStatus | Badge text | Color |
   |---|---|---|
   | CLEAN | `Ready to merge` | green |
   | HAS_HOOKS | `Ready (hooks)` | green |
   | UNSTABLE | `Checks failing/pending` | yellow |
   | BEHIND | `Behind base` | yellow |
   | BLOCKED | `Blocked (reviews/rules)` | red |
   | DIRTY / mergeable=CONFLICTING | `Conflicts` | red |
   | UNKNOWN | `Computing…` | grey (refetch detail once after 2 s, max 3 times) |

   Then review decision badge: `Approved` green / `Changes requested` red / `Review required` yellow.
3. **Meta**: `owner/repo`, `head → base` (prefix head with `fork:` when `isCrossRepository`),
   `by @author · opened <rel> · updated <rel>`, `+additions −deletions · N files`,
   `auto-merge: SQUASH` if `autoMergeRequest` is set.
4. **Labels** (`[name]` chips colored from the label hex), **Assignees**, **Reviewers**: requested
   reviewers (`@login (requested)`, teams as `@org/slug`) plus `latestReviews` entries as
   `@login ✓ approved / ✗ changes requested / ● commented`. Omit empty sections.
5. **Checks**: header `Checks: <rollup state> (<ok>/<total>)`, then one line per context:
   `✓ name  (workflow)  2m13s` / `✗ name` / `● name (in progress)` / `○ name (skipped/neutral)`.
   Required checks get a `*` suffix. Clicking a check line opens its `detailsUrl`/`targetUrl`.
   Section is collapsible (click header or press `C`); default expanded when rollup != SUCCESS.
6. **Body**: `body` with HTML comments (`<!-- … -->`) removed, >2 consecutive blank lines collapsed,
   fenced code blocks shown dim, headings bold. Empty body → dim `(no description)`.
7. **Timeline**: `Conversation (N)` header, then items sorted ascending by timestamp:
   - Issue comment: header `@login · <rel> [bot]` (bot = `__typename == "Bot"` or login ends with
     `[bot]`), body indented 2 cols, minimized comments collapsed to `(hidden: <reason>)`.
   - Review: header `@login <state glyph+word> · <rel>` then body (if any), then each inline review
     comment as `  ↳ path:line — first body line…` (full body shown when section expanded via click/`C`;
     later: full thread with `diffHunk`).
   - Later: event one-liners from `timelineItems` (commits, force-push, merged, closed, reopened,
     ready-for-review, review requested).
   Timestamp for a review is `submittedAt`; for a comment `createdAt`. The newest item is at the bottom;
   `G` jumps there. Only the last 50 comments and 40 reviews are loaded; if `totalCount` exceeds
   that, show `… N earlier items not loaded (open in browser)`.

While the detail is loading, the pane shows the row's title/repo immediately plus `Loading details…`;
previously loaded content for the same PR stays visible during a refresh.

---

## 4. Actions and flows

| Action | Key | Mouse | Availability | Flow |
|---|---|---|---|---|
| Open detail | Enter | double-click row / single-click row in sidebar | always | switch screen, fetch detail |
| Refresh | r | footer chip | always | refetch list (same cursor) and current detail |
| Scope picker | s | header scope chip | always | modal (2.4) |
| State filter | f cycles; 1–4 direct | header tabs | always | refetch |
| Open in browser | o | click title / footer chip | PR selected | `open`/`xdg-open`/`cmd /c start` on `url` |
| Comment | c | footer chip | PR selected | composer (4.1) → `addComment` → toast → refetch detail |
| Merge menu | m | footer chip / mergeable badge | PR open | merge menu (4.2) → confirm (4.3) → mutation → refetch list+detail |
| Close / Reopen | in merge menu, or `x` | menu item | `viewerCanClose` / `viewerCanReopen` | confirm → mutation → refetch |
| Approve / Request changes (nice-to-have, cheap) | a / X | footer chip | PR open, not `viewerDidAuthor` for approve | composer in review mode (body optional) → `addPullRequestReview` with `event` |
| Copy checkout command (nice-to-have, cheap) | y | footer chip | PR selected | `gh pr checkout <n> -R owner/repo` written as OSC 52 to stdout; toast shows the command either way |
| Toggle checks/timeline expand | C | click section header | detail focused | local |
| Help | ? | footer chip | always | modal listing keybindings for the current context |
| Quit | q / Ctrl-c | – | always (q ignored inside composer) | restore terminal |

### 4.1 Comment composer

- Popup anchored to the bottom of the detail pane (height 40 % of pane, min 8 rows, full pane width),
  bordered, title `Comment on #98 — Ctrl+S send · Esc cancel · @ mention`. Multi-line editor
  (`editor.rs`): insert/delete, arrows, Home/End, Ctrl+A/E, Ctrl+W delete word, Ctrl+U clear line,
  Enter = newline, bracketed paste inserts text verbatim, word-wrap at width, cursor shown via
  `Frame::set_cursor_position`.
- Submit = **Ctrl+S** (Ctrl+Enter is not distinguishable from Enter in most terminals without the
  kitty keyboard protocol; accept it as a bonus if `KeyboardEnhancementFlags` are supported).
  Empty/whitespace-only body: toast `Nothing to send`.
- Esc with non-empty text asks `Discard comment? [y/n]`; draft is kept per PR id in `App` so a
  cancelled composer can be reopened with the draft intact (cleared after successful send).
- While sending, editor is read-only with title `Sending…`; on error, the editor reopens with the
  text and the error in the status line.
- **@mention autocomplete**: when the caret is preceded by `@` + `[A-Za-z0-9-/]*` and the char before
  `@` is start-of-text or whitespace, show a popup (max 8 rows) directly under the caret row
  (above it when there is no room). Candidates, deduplicated case-insensitively, ranked by
  (prefix match > substring match, then source order):
  1. configured bots (`config.bots`) merged with defaults `claude`, `copilot`, `coderabbitai`
  2. authors of loaded comments/reviews (this is the only place Bot-type accounts such as
     `coderabbitai` appear — see 7.6)
  3. `participants` (users) of the PR
  4. `repository.mentionableUsers(first: 100)` preloaded with the detail
  5. later: remote `mentionableUsers(query:)` after 250 ms debounce when typed prefix >= 2 chars
     and fewer than 3 local matches
  ↑/↓ or Ctrl+N/P move, Tab or Enter accepts (inserts `@login `), Esc closes popup only, continuing
  to type refilters. Mouse: click a row accepts, wheel scrolls, click elsewhere closes popup.

### 4.2 Merge menu

Centered modal (width 44, height items+4), title `Merge #98 into main`. Items, each with an
accelerator letter and a dim subtitle:

| Item | Shown when | Subtitle |
|---|---|---|
| `m` Create a merge commit | `repository.mergeCommitAllowed` | `viewerMergeHeadlineText(mergeType: MERGE)` |
| `s` Squash and merge | `repository.squashMergeAllowed` | `viewerMergeHeadlineText(mergeType: SQUASH)` |
| `r` Rebase and merge | `repository.rebaseMergeAllowed` | disabled (dim) with `cannot be rebased cleanly` when `canBeRebased == false` |
| `x` Close pull request | state OPEN and `viewerCanClose` | – |
| `p` Reopen pull request | state CLOSED and `viewerCanReopen` | – |

Merge items are shown but disabled (dim, not selectable) when `mergeStateStatus` is DIRTY, BLOCKED, or
`mergeable == CONFLICTING`, with the reason as subtitle; `viewerCanMergeAsAdmin` re-enables BLOCKED with
subtitle `admin override`. A top line shows the current badge (`Ready to merge`, `Checks failing`, …).
Enter/click runs the item → confirmation. Esc/click-outside closes.

### 4.3 Confirmation dialog

Centered, width 56: title (`Squash and merge #98?`), 2–4 info lines (repo, `head → base`,
commit headline that will be used, `Branch will be deleted automatically` if `deleteBranchOnMerge`),
and two buttons `[ Confirm ]` `[ Cancel ]`. Enter/`y` confirms, Esc/`n` cancels, Tab/←/→ or click
switch buttons. Merge mutations send `expectedHeadOid = headRefOid` from the loaded detail; a
"head changed" error produces toast `#98 changed since loaded — refreshed, please review and retry`
and refetches. Later: make the commit headline/body editable in this dialog.

### 4.4 Refresh and auto-refresh

- Manual `r` refetches the list page(s) currently loaded (first page only if > 3 pages) and the detail.
- Auto-refresh every `config.refresh_secs` (default 60, 0 disables) via `tokio::time::interval`,
  skipped while a modal/composer is open or a mutation is in flight; it never resets scroll or selection.
- Successful mutations trigger an immediate refetch of the detail (and the list for merge/close/reopen).

---

## 5. Keybindings and mouse

Keys are matched on `KeyEventKind::Press` only (Windows sends Release too).

### 5.1 Keyboard

| Context | Key | Action |
|---|---|---|
| Global (no modal/composer) | q, Ctrl-c | quit |
| | ? | help modal |
| | s | scope modal |
| | f / 1 2 3 4 | cycle filter / set Open, Closed, Merged, All |
| | r | refresh |
| | o | open selected PR in browser |
| | c | comment composer |
| | m | merge menu |
| | y | copy checkout command (nice-to-have) |
| List screen & sidebar | ↓/j, ↑/k | move selection |
| | PgDn/PgUp, Ctrl-d/Ctrl-u | move by page / half page |
| | g/Home, G/End | first / last row (End also triggers load-more) |
| | Enter | open detail (list) / focus detail pane (sidebar) |
| Detail screen | Esc, Backspace | back to list screen |
| | Tab, h/l, ←/→ | toggle focus sidebar ⇄ detail |
| Detail pane focused | j/k, ↓/↑ | scroll 1 line |
| | PgDn/PgUp, Ctrl-d/Ctrl-u, Space | page / half page |
| | g/G | top / bottom (newest comment) |
| | C | toggle expand of checks/timeline section under cursor row (or all) |
| | a / X | approve / request changes (nice-to-have) |
| Scope modal | printable, Backspace | edit filter |
| | ↑/↓, Ctrl-p/n | move |
| | Enter | select; Esc close |
| Merge menu | ↑/↓/j/k, Enter, Esc | move, run, close; accelerators m s r x p |
| Confirm | Enter/y, Esc/n, Tab/←/→ | confirm, cancel, switch button |
| Composer | printable, Enter, Backspace, Delete, arrows, Home/End, Ctrl-a/e/w/u | edit |
| | Ctrl-s (and Ctrl-Enter when reported) | send |
| | Esc | close mention popup if open, else cancel (confirm if non-empty) |
| Mention popup | ↑/↓, Ctrl-p/n, Tab/Enter, Esc | move, accept, close |
| Help modal | any key / click | close |

### 5.2 Mouse

Hit-testing: `ui::draw` writes every interactive Rect into `app.hits: HitMap` (fields like
`scope_chip`, `filter_tabs: Vec<(Filter, Rect)>`, `list_area`, `list_rows: Vec<(usize, Rect)>`,
`sidebar_area`, `detail_area`, `detail_lines: Vec<(usize, LineHit)>` for clickable lines such as
title/checks/section headers, `footer_chips: Vec<(Action, Rect)>`, `modal: Option<Rect>`,
`modal_rows: Vec<(usize, Rect)>`, `buttons: Vec<(Action, Rect)>`, `composer_area`,
`mention_rows`). The mouse handler converts `(column,row)` to `Position` and tests
`Rect::contains`, modal first, then regions. A double-click is two `Down(Left)` within 400 ms on
the same row. Actions happen on `Down(Left)` for rows/tabs and on `Up(Left)` for buttons (so a press
that drags off cancels). Wheel events scroll whatever is **under the pointer**, not the focused pane.

| Context | Event | Target | Action |
|---|---|---|---|
| Any | Down(Left) | header scope chip | open scope modal |
| | Down(Left) | header filter tab | set filter |
| | Down(Left) | footer chip | run chip's action |
| | Down(Left) | status line | clear toast |
| List screen | Down(Left) | row | select row |
| | double-click | row | open detail |
| | Wheel | table | scroll selection view by 3 rows (moves highlight if it leaves view) |
| | Down(Left) | scrollbar track | jump proportionally (nice-to-have) |
| Detail screen | Down(Left) | sidebar row | focus sidebar + select (loads detail) |
| | Down(Left) | detail pane | focus detail; if on title → open browser; on check line → open details URL; on section header → toggle expand |
| | Wheel | sidebar / detail | scroll that pane by 3 lines |
| | Drag(Left) | detail scrollbar | drag-scroll (nice-to-have) |
| Modal (scope/merge/confirm/help) | Down(Left) | row / button | select or run |
| | Down(Left) | outside modal | dismiss (same as Esc; composer asks to discard if non-empty) |
| | Wheel | modal list | scroll |
| Composer | Down(Left) | text area | place caret (row/col mapped through wrap map) |
| | Down(Left) | title-bar `Send`/`Cancel` chips | send / cancel |
| | Wheel | text area | scroll editor |
| Mention popup | Down(Left) / Wheel | row | accept / scroll |
| Any | Moved | – | ignored (no hover state in MVP) |

---

## 6. Runtime architecture

```
std thread: crossterm::event::read() ──► mpsc::UnboundedSender<Event>
tokio tasks (reqwest)                ──► mpsc::UnboundedSender<Msg>
main loop (tokio::select!): { term events, Msg, tick(refresh), tick(spinner 80ms) } → app.update() → ui::draw()
```

- Render only when something changed (`app.dirty`) or every 80 ms while a spinner is visible.
- `Msg` variants: `ViewerLoaded{gen, login, orgs}`, `PrsLoaded{gen, page, rows, page_info, issue_count, rate}`,
  `DetailLoaded{gen, pr_id, detail, rate}`, `MentionsLoaded{gen, users}`, `MutationDone{kind, pr_id, result}`,
  `Error{gen, context, message}`.
- Generation counters: `list_gen` (bumped on scope/filter change and manual refresh), `detail_gen`
  (bumped per selected PR id change / refresh), `mention_gen`. Any result whose `gen` != current is dropped.
- Terminal setup: `ratatui::init()` then `execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)`;
  on exit (including panic hook) the reverse plus `ratatui::restore()`.
- Auth: `GITHUB_TOKEN`, else `GH_TOKEN`, else `tokio::process::Command::new("gh").args(["auth","token"])`
  stdout trimmed. No token → exit with a clear message before entering the TUI.
- HTTP: `POST https://api.github.com/graphql`, headers `Authorization: Bearer <token>`,
  `User-Agent: gh-pr-manager/<version>` (GitHub rejects requests without one), `Content-Type: application/json`.
  Body `{"query": "...", "variables": {...}}`. Treat HTTP 200 with a non-empty `errors` array as failure;
  surface `errors[0].message` (`type` is `NOT_FOUND`/`FORBIDDEN`/`UNPROCESSABLE` for mutations).
  Timeout 30 s. On 401 → `Token rejected — check GITHUB_TOKEN / gh auth login`. On 403/429 with
  `Retry-After` → back off that many seconds and show it.

---

## 7. GraphQL

All PR queries fetch `rateLimit { remaining resetAt cost }` alongside; it costs nothing extra.

### 7.1 Viewer and organizations
```graphql
query Viewer {
  viewer { login organizations(first: 100) { nodes { login } } }
  rateLimit { remaining resetAt cost }
}
```

### 7.2 Search PRs (list, paginated)
```graphql
query SearchPrs($q: String!, $first: Int!, $after: String) {
  search(query: $q, type: ISSUE, first: $first, after: $after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes { ... on PullRequest {
      id number title url state isDraft merged updatedAt createdAt
      author { login __typename }
      repository { nameWithOwner }
      reviewDecision isReadByViewer totalCommentsCount
      headRefName baseRefName
      statusCheckRollup { state }
    } }
  }
  rateLimit { remaining resetAt cost }
}
```
`statusCheckRollup` directly on `PullRequest` is valid (no need for `commits(last:1)`); it is `null`
when there are no checks. `author` is `null` for deleted users → show `ghost`.

### 7.3 PR detail by node id
```graphql
query PrDetail($id: ID!) {
  node(id: $id) { ... on PullRequest {
    id number title url state isDraft merged closed locked
    createdAt updatedAt mergedAt closedAt body
    author { login __typename }
    headRefName baseRefName headRefOid isCrossRepository
    headRepository { nameWithOwner }
    mergeable mergeStateStatus canBeRebased reviewDecision
    viewerCanUpdate viewerDidAuthor viewerCanMergeAsAdmin viewerCanClose viewerCanReopen
    additions deletions changedFiles
    autoMergeRequest { mergeMethod }
    mergeHeadline: viewerMergeHeadlineText(mergeType: MERGE)
    squashHeadline: viewerMergeHeadlineText(mergeType: SQUASH)
    repository {
      nameWithOwner mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed
      deleteBranchOnMerge viewerPermission
      mentionableUsers(first: 100) { nodes { login } }
    }
    labels(first: 20) { nodes { name color } }
    assignees(first: 10) { nodes { login } }
    reviewRequests(first: 10) { nodes { requestedReviewer {
      __typename ... on User { login } ... on Team { slug } ... on Bot { login } } } }
    participants(first: 50) { nodes { login } }
    latestReviews(first: 20) { nodes { author { login } state submittedAt } }
    statusCheckRollup {
      state
      contexts(first: 50) { totalCount nodes {
        __typename
        ... on CheckRun { name status conclusion detailsUrl startedAt completedAt
                          isRequired(pullRequestId: $id)
                          checkSuite { app { name } workflowRun { workflow { name } } } }
        ... on StatusContext { context state targetUrl description createdAt
                               isRequired(pullRequestId: $id) }
      } }
    }
    comments(last: 50) { totalCount nodes {
      id author { login __typename } body createdAt url isMinimized minimizedReason } }
    reviews(last: 40) { totalCount nodes {
      id author { login __typename } state body submittedAt url
      comments(first: 20) { totalCount nodes {
        id author { login } body path line originalLine outdated createdAt url } } } }
  } }
  rateLimit { remaining resetAt cost }
}
```
Later (events in timeline): add
`timelineItems(last: 60, itemTypes: [PULL_REQUEST_COMMIT, MERGED_EVENT, CLOSED_EVENT, REOPENED_EVENT,
READY_FOR_REVIEW_EVENT, CONVERT_TO_DRAFT_EVENT, REVIEW_REQUESTED_EVENT, HEAD_REF_FORCE_PUSHED_EVENT])
{ nodes { __typename ... on PullRequestCommit { commit { abbreviatedOid messageHeadline committedDate } }
... on MergedEvent { createdAt actor { login } } ... on ClosedEvent { createdAt actor { login } }
... on ReopenedEvent { createdAt actor { login } } ... on ReadyForReviewEvent { createdAt actor { login } }
... on ConvertToDraftEvent { createdAt actor { login } } ... on HeadRefForcePushedEvent { createdAt actor { login } }
... on ReviewRequestedEvent { createdAt actor { login } requestedReviewer { ... on User { login } } } } }`
(validated). Later (threads): `reviewThreads(first: 50) { nodes { id isResolved isOutdated path comments(first: 20) {...} } }`.

### 7.4 Remote mention search (later)
```graphql
query Mentionable($owner: String!, $name: String!, $q: String!) {
  repository(owner: $owner, name: $name) {
    mentionableUsers(first: 10, query: $q) { nodes { login name } }
  }
}
```

### 7.5 Mutations (input fields validated by introspection)
```graphql
mutation AddComment($subjectId: ID!, $body: String!) {
  addComment(input: {subjectId: $subjectId, body: $body}) { commentEdge { node { id url createdAt } } }
}

mutation MergePr($id: ID!, $method: PullRequestMergeMethod!, $expectedHeadOid: GitObjectID,
                 $headline: String, $commitBody: String) {
  mergePullRequest(input: {pullRequestId: $id, mergeMethod: $method, expectedHeadOid: $expectedHeadOid,
                           commitHeadline: $headline, commitBody: $commitBody}) {
    pullRequest { id state merged mergedAt mergeStateStatus }
  }
}
# PullRequestMergeMethod = MERGE | SQUASH | REBASE. Omit commitHeadline/commitBody (null) to use GitHub defaults.

mutation ClosePr($id: ID!)  { closePullRequest(input: {pullRequestId: $id})  { pullRequest { id state closed closedAt } } }
mutation ReopenPr($id: ID!) { reopenPullRequest(input: {pullRequestId: $id}) { pullRequest { id state closed } } }

mutation AddReview($id: ID!, $event: PullRequestReviewEvent!, $body: String) {
  addPullRequestReview(input: {pullRequestId: $id, event: $event, body: $body}) {
    pullRequestReview { id state url }
  }
}
# PullRequestReviewEvent = COMMENT | APPROVE | REQUEST_CHANGES | DISMISS. REQUEST_CHANGES/COMMENT require a body.
```

Enums to model: `PullRequestState {OPEN CLOSED MERGED}`, `MergeableState {MERGEABLE CONFLICTING UNKNOWN}`,
`MergeStateStatus {DIRTY UNKNOWN BLOCKED BEHIND UNSTABLE HAS_HOOKS CLEAN}`,
`PullRequestReviewDecision {CHANGES_REQUESTED APPROVED REVIEW_REQUIRED}`,
`PullRequestReviewState {PENDING COMMENTED APPROVED CHANGES_REQUESTED DISMISSED}`,
`StatusState {EXPECTED ERROR FAILURE PENDING SUCCESS}`,
`CheckStatusState {REQUESTED QUEUED IN_PROGRESS COMPLETED WAITING PENDING}`,
`CheckConclusionState {ACTION_REQUIRED TIMED_OUT CANCELLED FAILURE SUCCESS NEUTRAL SKIPPED STARTUP_FAILURE STALE}`.
Parse all enums with `#[serde(other)] Unknown` fallbacks so new values never break deserialization.

### 7.6 Schema findings worth knowing
- `repository.mentionableUsers` returns **only User accounts**: `copilot`, `coderabbitai` and other
  GitHub Apps are never returned, and `query:` matches login/name prefix only. Bot handles must come
  from the configured bot list and from `author { login __typename }` of loaded comments/reviews
  (where `__typename == "Bot"` for e.g. `coderabbitai`, `chatgpt-codex-connector`).
- `participants` is a `UserConnection` too — Bot reviewers are absent even when they reviewed.
- `mergeStateStatus`, `viewerCanMergeAsAdmin`, `canBeRebased`, `viewerMergeHeadlineText/BodyText(mergeType:)`,
  `viewerCanClose`, `viewerCanReopen`, `isRequired(pullRequestId:)` all work with a plain `repo`-scoped
  OAuth token; no preview headers needed any more.
- `statusCheckRollup` lives directly on `PullRequest`; `contexts` is a union of `CheckRun` and `StatusContext`
  with different field names (`name/status/conclusion/detailsUrl` vs `context/state/targetUrl`).
- `search(type: ISSUE)` is the right type for PRs; `is:merged`, `is:unmerged`, `involves:@me`,
  `review-requested:@me`, `assignee:@me`, `user:<login>`, `org:<org>` all validated. `issueCount` is the
  total across pages. GitHub caps search at 1000 results.
- `viewer.organizations` only lists orgs the token can see (`read:org` scope); without it the list is empty,
  not an error.
- `addComment` takes `subjectId` (not `pullRequestId`). Mutation payloads: `commentEdge`, `pullRequest`,
  `pullRequestReview`.

---

## 8. Error handling, empty/loading states, rate limit, resize

| Situation | Behaviour |
|---|---|
| No token | Print `No GitHub token: set GITHUB_TOKEN or run gh auth login` to stderr, exit 1 before TUI |
| Network/timeout error on list | Keep existing rows, red toast `List refresh failed: <msg>`; empty-list error state if no rows |
| Error on detail | Pane shows red `Failed to load #98: <msg>` + `r retry`; previous content kept if any |
| GraphQL `errors[]` | Message from `errors[0].message`; for mutations prefix with the action (`Merge failed: …`) |
| Mutation rejected (not mergeable, head changed, permissions) | Toast persists; detail refetched so badges reflect reality |
| 401 | Toast with auth hint, auto-refresh paused until manual `r` |
| 403/429 secondary limit | Toast `Rate limited, retrying in Ns`, auto-refresh paused for that long |
| `rateLimit.remaining` | Header `api N`; yellow < 1000, red < 300; when 0, show `resets <HH:MM>` and disable auto-refresh |
| Stale results | Dropped by generation mismatch, never rendered |
| Loading | Spinner in header; per-region loading text per sections 1.2 / 3 |
| Resize | `Event::Resize` marks dirty; layout is recomputed every frame from `frame.area()`; scroll offsets and modal sizes are clamped after layout; the wrap cache is keyed by width so the detail re-wraps |
| Panic | Panic hook restores the terminal (disable mouse capture, leave alt screen) before printing |
| Clipboard unavailable | OSC 52 is best-effort; the toast always shows the command text |

---

## 9. Config

`$XDG_CONFIG_HOME/gh-pr-manager/config.toml`, fallback `~/.config/gh-pr-manager/config.toml`; missing file = defaults;
a parse error aborts startup with the TOML error.

```toml
refresh_secs = 60                 # 0 disables auto-refresh
bots = ["claude", "copilot", "coderabbitai", "cursor", "gemini-code-assist"]
default_scope = "My PRs"          # optional, by name
page_size = 50                    # optional, 10..100

[[scopes]]
name = "Team backend"
query = "org:stealth-factory label:backend"

[[scopes]]
name = "Dependabot"
query = "author:app/dependabot"
```

---

## 10. Module layout

| File | Responsibility |
|---|---|
| `src/main.rs` | arg/env parsing, config load, token resolution, terminal init/restore + panic hook, spawn input thread, run loop with `tokio::select!` |
| `src/app.rs` | `App` state (screen, focus, modal, rows, selection by id, detail, scroll, drafts, generations, toast, `hits`), `update(Event)`, `apply(Msg)`, action dispatch |
| `src/model.rs` | domain structs/enums (`PrRow`, `PrDetail`, `Check`, `TimelineItem`, `Scope`, `Filter`, enums with `Unknown` fallback), `search_query()` builder, relative-time helper |
| `src/github.rs` | `GhClient` (reqwest, token, UA), `graphql<T>(query, vars)`, typed fetch/mutation fns, error type, rate-limit extraction |
| `src/queries.rs` | the GraphQL strings from section 7 as `const &str` |
| `src/events.rs` | input thread, `Msg` enum, spawn helpers that tag results with generations |
| `src/ui/mod.rs` | `draw(frame, &mut App)`: header, footer, toast, screen switch, modal overlay; fills `HitMap` |
| `src/ui/list.rs` | table/sidebar rendering, column layout |
| `src/ui/detail.rs` | detail document builder + wrap cache + clickable line map |
| `src/ui/modals.rs` | scope picker, merge menu, confirm, help, composer frame, mention popup |
| `src/editor.rs` | multi-line text buffer: cursor, edit ops, wrap map, mention-trigger detection |
| `src/mentions.rs` | candidate collection/ranking from bots, authors, participants, mentionable users |
| `src/config.rs` | TOML config structs, path resolution, defaults |
| `src/hit.rs` | `HitMap` struct and `hit_test(Position) -> Option<Target>` |

Unit tests to include: `search_query()` composition, mention trigger parsing, timeline merge order,
relative time, word wrap widths, `HitMap` lookup precedence (modal over body).

---

## 11. Prioritisation

**MVP (ship first)**
1. Token resolution, viewer+orgs, built-in + config scopes, scope modal with type-to-filter, state filter
2. List screen with table, pagination, selection by id, refresh, loading/empty/error states, spinner, rate display
3. Detail split screen: all section-3 content except timeline events/threads; scrolling; focus switching
4. Comment composer with @mention autocomplete (local sources); `addComment`
5. Merge menu from repo flags + close/reopen; confirmation; `expectedHeadOid`; refetch after mutation
6. Open in browser; full keyboard table; full mouse table (rows, tabs, chips, modals, click-outside, wheel)
7. Auto-refresh with generation guard; toasts; resize; panic-safe restore

**Later (nice-to-have)**
- Approve / request changes via `addPullRequestReview`
- Copy checkout command (OSC 52)
- Editable commit headline/body in merge confirm; delete branch after merge (`deleteRef` with `headRef { id }`)
- Timeline events from `timelineItems`; review threads with `diffHunk` and resolved state
- Remote `mentionableUsers(query:)` autocomplete; `participants` paging
- Local `/` filter over loaded rows; column sort; scrollbar click/drag; hover highlight
- Mark-as-read indicator from `isReadByViewer`; unread count in header
- Enable/disable auto-merge (`enablePullRequestAutoMerge`) when `viewerCanEnableAutoMerge`
