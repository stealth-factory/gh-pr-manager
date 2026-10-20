# "What changed" signals — design

Request: "ask fable on other 'what changed' signal and see its recommendations". Today a refresh (list every 60 s, open PR every
20 s, Actions every 10 s, `r`, and the refetch after your own mutation) silently swaps the data; nothing says a comment landed or
CI went red. The goal is polling that *feels* pushed: every refresh is diffed against what you last saw, and the diff is shown in
the place you are looking at. All GraphQL below was validated read-only with `gh api graphql` on 2026-10-19 (account `wiiiimm`,
scopes `repo read:org workflow gist`); timings are wall-clock and include ~0.5 s of `gh` overhead; cost is `rateLimit.cost`.

## 1. What is worth signalling (ranked by user value)

| Tier | Event | Detected from | Notes |
|---|---|---|---|
| A — interrupt-worthy (bell-eligible) | your review is requested | `viewerLatestReviewRequest` null → non-null (list); `reviewRequests` gains the viewer (detail) | also "PR appeared in *Review requested* scope" |
| A | CI went red on your PR | `ci_state()` `PENDING/SUCCESS` → failing | failing = the `✗` set of `ci_icon` |
| A | verdict on your PR: approved / changes requested | `reviewDecision` change; detail: latest non-COMMENTED review per reviewer | |
| A | merged or closed under you | `state` `OPEN` → `MERGED/CLOSED` on a PR you authored or are reading | banner in the detail pane |
| A | conflicts appeared / blocked → clean | `mergeable` `MERGEABLE` → `CONFLICTING` (list); `mergeStateStatus` `BLOCKED/BEHIND` → `CLEAN` (detail) | `UNKNOWN` is never a change |
| B — badge-worthy, counted | new comments | `comments.totalCount` +n (list); detail refines by author | bots and yourself excluded where the author is known |
| B | new review threads / review submissions | `reviewThreads.totalCount`, `reviews.totalCount` +n | empty COMMENTED containers count both ways, so deltas stay consistent |
| B | commits pushed | `headRefOid` changed; `commits.totalCount` delta (0 on force-push) | on PRs you review; implies Changes/Checks are stale |
| B | draft → ready for review; description edited | `isDraft` true → false; `lastEditedAt` changed | |
| B | PR entered / left the scope | id new to a complete list / gone from it | "left" only when the list is complete (not cut at `MAX_PRS`, same query) |
| C — noise, update silently | `updatedAt` bump alone (labels, projects, bot edits, re-runs); CI `→ PENDING` or `→ SUCCESS` on others' PRs (green on *yours* is tier B); counts going *down*; title edits; your own actions | | never a dot |

Bots: the data is bot-heavy (`latestReviews` on the org's PRs is coderabbit/greptile/codex). `cfg.bots` already lists them;
detail-level diffs drop entries by those logins (and `[bot]` suffixes) from "new" counts; list-level counts cannot tell, so a
row's `+2` means "activity", and the detail refines it to "1 new (1 bot)".

## 2. Detection: diff snapshots we already fetch

**`SEARCH_QUERY` additions** (measured on `is:open org:stealth-factory`, 50 nodes, interleaved with today's query):

| Variant | Latency | Payload | Cost |
|---|---|---|---|
| today | 4.3–6.2 s | 24.7 KB | 1 |
| + `reviews{totalCount} reviewThreads{totalCount} reviewRequests{totalCount} headRefOid mergeable commits{totalCount}` | 4.7–6.5 s (+0.2–0.3 s, within noise) | 34.2 KB | 1 |
| + also `lastEditedAt isReadByViewer viewerLatestReviewRequest{asCodeOwner}` (**recommended**) | 5.4–6.9 s (no consistent penalty) | 38.5 KB (+14 KB) | 1 |
| + nested `latestReviews(first:10) reviewRequests(first:10){nodes} labels(first:10)` | 6.1–7.8 s | 49 KB | **2** — rejected |

So: add only scalars and `totalCount`s to the list query (cost stays 1, +14 KB/page); `commits(last:1)` keeps its rollup and
gains `totalCount`. `mergeable` came back `MERGEABLE`/`CONFLICTING` for all 50 (treat `UNKNOWN` as "no change"; GitHub computes
it lazily). `isReadByViewer` is GitHub's own notification read-state and is free: 17 of 50 unread in `involves:@me`, 12 of 50 in
the org scope; `viewerLatestReviewRequest` was set on 2 of 50.

**`DETAIL_QUERY` additions**, cost unchanged (1; a stripped probe took 1.05 s / 7.3 KB): `totalCount` on `comments`, `reviews`,
`reviewThreads` (so detail and list snapshots agree), `id` on comments/reviews/threads, `oid` on commits, `lastEditedAt`,
`isReadByViewer`, and `timelineItems(last:5, itemTypes:[MERGED_EVENT, CLOSED_EVENT, REVIEW_REQUESTED_EVENT,
HEAD_REF_FORCE_PUSHED_EVENT, READY_FOR_REVIEW_EVENT])` for actors ("merged by alice") — the last one is a later nicety.

**Snapshot + diff.** One `Snapshot` struct (the signal fields only) is built from a `PrSummary` *or* a `PrDetail`; one `diff(old,
new)` serves three purposes: the refresh digest (old = previous summary), the unread state (old = persisted baseline), and the
timeline markers (old = baseline frozen when the PR was opened). `Msg::Prs` keeps the previous list (`mem::take`) and diffs each
page by id against it; "left the scope" is computed once the last page lands and only when `total == prs.len()`. A list arriving
for a different `search_query()` (scope/state/repo change) updates baselines but produces no digest.

**The "seen" model** — what counts as read:
- `state.seen: HashMap<pr id, Snapshot>` is persisted in `state.json`, so "new since you last looked" works across sessions.
  A row is *unread* when `diff(seen[id], current)` is non-empty. `Snapshot.updated_at` is the PR's `updatedAt` at the time you
  looked, which is the time cursor for timeline markers (a comment with `createdAt > seen.updated_at` is new; wall-clock is never
  used, so there is no clock-skew issue).
- **Opening a PR marks the row read** (`OpenSelected`, `J`/`K`, sidebar click): the summary-level snapshot is stored at once (dot
  clears), the detail-level one when `Msg::Detail` lands. Selecting a row in the list without opening does not mark it.
- **Per tab:** tab badges (`+2`) clear when that tab is shown (`SetTab`); the `NEW` markers inside the tab stay for as long as the
  PR is open (`viewing_since` holds the baseline frozen at open) so you can still find them, and go when you leave the PR.
- **Own actions never count.** `Msg::Done` puts the id in `own_pending`; the list page and the detail that follow update `seen`
  from the fresh data and drop the id from the digest. Detail-level diffs also skip comments/reviews authored by the viewer.
- **First run (no baseline):** quiet start — the first snapshot becomes the baseline, nothing is dotted, *except* PRs GitHub
  itself reports `isReadByViewer == false` (seeded unread, marked "unread on GitHub" in the digest). After that, `isReadByViewer`
  is only consulted on a `false → true` edge: you read it on github.com, so clear the local unread too. The TUI cannot set it
  (that needs the notification thread id and the `notifications` scope), so it is a hint, never the source of truth.
- Entries are evicted on save: cap 2000 ids, drop those untouched for 30 days.

## 3. Where and how to show it

**List rows** (plain text + colour, no emoji): an accent `•` in the existing 2-column gutter, a bold title, and deltas as numbers
next to the counts they belong to; a changed CI/Review cell is bold. Cmts widens from 4 to 7 columns.

```
   Repository                #     Title                                   Author    CI  Review      Cmts     Updated
 • ● stealth-factory/form-atlas #151 SWR-899: app skin deforms like a human  wiiiimm   ✗   —           3 +2    3m
   ● stealth-factory/opscope    #439 fix(core): tmux pixel charts            copilot   ✓   ✓ approved 20       1d
 • ● stealth-factory/opscope    #437 chore(deps): bump tokio                 renovate  ●   ◔ review    2 +1    8m
```
Comfortable density (rows are 2 high already) uses the second line for words instead of a blank:
```
 • ● stealth-factory/form-atlas #151 SWR-899: app skin deforms like a human  wiiiimm   ✗   —           3 +2    3m
       CI failed · 2 new comments · 3 commits pushed
```
Compact keeps `•` and the `+n` (both are text) and drops nothing else.

**Sidebar** (detail view): same `•` before the state icon; the dim second line (Cozy/Comfortable) ends with the top two changes.
```
▌• ● ✗ #151 SWR-899: app skin deforms like a human s…
      form-atlas · wiiiimm · 3m · CI failed · +2 comments
  ● ✓ #439 fix(core): tmux pixel charts
      opscope · copilot · 1d
```

**Tab badges**: the count stays, a `+n` (accent, bold; on the active tab bold only) follows it; Description shows `✎` when the
body was edited; Checks/Actions show `+n` for contexts/runs whose outcome changed.
```
 1 Description ✎  2 Conversation 7 +2  3 Changes 12  4 Commits 14 +3  5 Checks 9 ✗ +1  6 Actions 4 ✗  7 Reviews 16 +1
```

**Timeline markers** (Conversation, Reviews, Commits): a divider before the first new entry and a `NEW` word on each, with the
gutter bar in yellow instead of accent (`▍` → `▍` recoloured; the word carries the meaning).
```
 ▍coderabbitai commented · 2h ago
 ▍ …
 ── 2 new since you last looked ───────────────────────────────────────────────
 ▍alice changes requested · 9m ago · NEW
 ▍ The retry loop still swallows the error…
```
A force-push (head changed, `commits.totalCount` unchanged) gets a banner at the top of Commits: `head moved d1e3597 → 9af01c2
(force-push)`. State changes while reading get a pane banner: `── Merged 2 min ago (by alice, once timeline actors are fetched) ──`.

**Refresh digest** on the status line (replaces the plain query text for 10 s, errors still win; `ToastKind::Changes`):
```
 ↻ 3 PRs changed · #151 CI failed · #437 review requested · #439 +2 comments        u shows unread · click to jump
```
Clicking it selects the first changed PR (opens it when already in the detail view). More than three items → `· +2 more`.

**Header**: ` 87 PRs · 4 unread ` (`4 unread` accent+bold, clickable = toggle the unread filter); with the filter on,
` 4 unread of 87 PRs ` and a `✕` to clear, like the repo chip.

**Bell / desktop notification** (off by default; tier-A events only; the status line is always the fallback):
```toml
[notify]
events = ["review_requested", "approved", "changes_requested", "ci_failed", "merged", "conflicts"]   # [] = off
mine_only = true            # only PRs you authored or were asked to review
bell = false                # BEL (\x07): most terminals flash or beep; works through any multiplexer
desktop = "off"             # "auto" | "osc9" | "osc777" | "osc99" | "off"
```
`auto` picks by terminal: iTerm2/WezTerm → OSC 9 (`ESC ] 9 ; text BEL`), WezTerm/urxvt/Ghostty → OSC 777 (`ESC ] 777 ; notify ;
title ; body BEL`), Kitty → OSC 99 (`ESC ] 99 ; i=1:d=0 ; text ESC \`), tmux → DCS pass-through only if `allow-passthrough` is
on, Herdr (`TERM_PROGRAM=herdr`) → unknown pass-through, so `auto` sends BEL only and the user opts into a sequence by name. Emit
through the same raw-stdout path as the OSC 52 copy (`app.rs::copy_to_clipboard`), never through the ratatui buffer. Rate-limit to
one notification per PR per 5 minutes; whether the terminal has focus is unknowable, so also skip them while a modal is open.

## 4. Interaction and keys

| Key | Does | Conflict check |
|---|---|---|
| `u` | toggle **unread only** (list and sidebar; header chip does the same) | free (`Ctrl+u` is half-page scroll, plain `u` unused; search palette binds `Ctrl-u` only inside itself) |
| `U` | mark all visible PRs read (toast `Marked 12 PRs read`); in the detail pane, just the open PR | free |
| `n` / `p` | next / previous `NEW` entry on Conversation, Reviews, Commits (wraps; toast `Nothing new here`) | `n`/`p` already mean "next/prev item of this tab" in Changes (files) and Actions (tree); same semantic |
| click a `+n` badge | switches to that tab and jumps to the first new entry | |
| click the digest | select / open the first changed PR | |

`Esc` on the list clears the unread filter before it clears the search filter. Help gains three rows; the footer gets `u Unread`.
Opening a PR is the only implicit "read"; a dwell timer for `J`/`K` skimming is listed under later.

## 5. Density and accessibility

Nothing relies on colour alone: `•` glyph + bold title for unread, `+n` numbers, the `NEW` word, `✎`, `↻`. Bold survives monochrome;
`•` and `+n` survive Compact; Comfortable spends its spare row on words. The digest is plain text on the status line and never
displaces an error toast or the log-search prompt.

## 6. Rust-level additions

```rust
// src/changes.rs (new, pure, unit-tested; no App access)
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot { pub updated_at: String, pub state: String, pub is_draft: bool, pub comments: usize, pub reviews: usize,
    pub threads: usize, pub review_requests: usize, pub commits: usize, pub head: String, pub ci: Option<String>,
    pub review: Option<String>, pub mergeable: Option<String>, pub edited_at: Option<String>, pub asked_me: bool }
impl Snapshot { pub fn of_summary(p: &PrSummary) -> Self; pub fn of_detail(d: &PrDetail, me: Option<&str>) -> Self; }
#[derive(Clone, PartialEq, Eq)]
pub enum Change { Appeared, Left { state: String }, State { from: String, to: String }, Ready, Edited,
    Comments(usize), Reviews(usize), Threads(usize), Commits { n: usize, head: String, force: bool },
    Ci { from: Option<String>, to: String }, Verdict { from: Option<String>, to: String }, ReviewRequested,
    Mergeable { from: Option<String>, to: String } }
impl Change { pub fn tier(&self) -> u8; pub fn label(&self) -> String /* "CI failed", "+2 comments" */;
    pub fn tab(&self) -> Option<Tab>; pub fn notify(&self, mine: bool) -> Option<&'static str> /* config event name */; }
pub fn diff(old: &Snapshot, new: &Snapshot) -> Vec<Change>;                       // sorted by tier, then field order
pub fn diff_list(old: &[PrSummary], new: &[PrSummary], complete: bool) -> Vec<(String, Vec<Change>)>;
pub fn diff_detail(old: &PrDetail, new: &PrDetail, me: Option<&str>, bots: &[String]) -> Vec<Change>;
pub struct NewMarks { pub comments: Vec<usize>, pub reviews: Vec<usize>, pub threads: Vec<usize>, pub commits: Vec<usize> } // indices
pub fn new_marks(d: &PrDetail, since: &Snapshot, me: Option<&str>, bots: &[String]) -> NewMarks;
pub fn digest(changes: &[(String, Vec<Change>)], prs: &[PrSummary], max: usize) -> String;
pub fn notification(ev: &[(String, Vec<Change>)], prs: &[PrSummary], me: &str, cfg: &NotifyConfig) -> Option<String>;
pub fn osc(kind: Desktop, title: &str, body: &str) -> String;                       // escape sequence builder

// github.rs — PrSummary gains (all #[serde(default)]): reviews: Count, review_threads: Count, review_requests: Count,
//   head_ref_oid: String, mergeable: Option<String>, last_edited_at: Option<String>, is_read_by_viewer: Option<bool>,
//   viewer_latest_review_request: Option<Value>; CommitConnection-style `commits: LastCommit { total_count, nodes }`.
//   PrDetail gains totalCounts (Nodes<T> → Counted<T> { total_count, nodes } for comments/reviews/review_threads),
//   last_edited_at, is_read_by_viewer, `id` on Comment/Review/ReviewThread, `oid` on CommitInfo.
//   impl PrSummary { pub fn absorb(&mut self, d: &PrDetail) }  // extends the title/state sync in Msg::Detail to the counters

// app.rs
pub unread: HashMap<String, Vec<Change>>,      // derived: diff(seen, current); rebuilt on Msg::Prs / Msg::Detail / mark_read
pub digest: Option<(Vec<(String, Vec<Change>)>, Instant)>,   // the last refresh's changes, for the status line and click
pub unread_only: bool, list_query: String, own_pending: HashSet<String>,
pub viewing_since: Option<(String, Snapshot)>, pub new_marks: Option<NewMarks>,  // frozen at open; drives NEW + n/p
fn mark_read(&mut self, id: &str, snap: Snapshot); fn mark_tab_read(&mut self, id: &str, tab: Tab);
Action::ToggleUnreadOnly | MarkAllRead | NextNew | PrevNew | JumpDigest; ToastKind::Changes (ttl 10 s)
// visible(): `unread_only` filters to ids in `unread` (after the text filter); selection kept by id via pending_select.

// state.rs — `pub seen: HashMap<String, Snapshot>` (#[serde(default)]; cap 2000, evict > 30 days on save)
// config.rs — `pub notify: NotifyConfig { events: Vec<String>, mine_only: bool, bell: bool, desktop: String }`
```
`state.json` shape: `"seen": { "PR_kwDO…": { "updated_at": "2026-10-19T05:34:41Z", "state": "OPEN", "comments": 7, "reviews": 11,
"threads": 5, "review_requests": 0, "commits": 14, "head": "d1e3597…", "ci": "FAILURE", "review": null, "mergeable": "MERGEABLE",
"edited_at": null, "is_draft": false, "asked_me": false } }`. Unknown fields default, so older state files load.

**Tests** (`changes.rs`, `app.rs`, `state.rs`): each `Snapshot` field transition yields exactly its `Change`; `UNKNOWN` mergeable,
decreasing counts, `→ PENDING` and an `updatedAt`-only bump yield nothing; force-push = head changed, `commits` equal; tier order;
`diff_list` matches by id across reorders, `Appeared`/`Left` only when `complete`; `of_summary` and `of_detail` agree on one PR (no
spurious unread when the detail replaces the list baseline); own-action flow in either arrival order (list page before or after the
detail) leaves no unread and no digest entry; first run seeds unread only from `isReadByViewer == false`, ignores it once a baseline
exists, and a `false → true` edge clears unread; open marks read, `SetTab` clears that tab only, markers persist until leaving;
`new_marks` excludes the viewer and `cfg.bots`, and `createdAt == seen.updated_at` is not new; `n`/`p` wrap; `u` composes with the
text filter and keeps the selection by id; `digest` truncates with `+n more`; `osc()` byte-exact for 9/777/99; state round-trip,
cap and 30-day eviction.

## 7. MVP versus later

**MVP (one PR, no behaviour change when nothing changed):** list-query scalars/`totalCount`s + `isReadByViewer` +
`viewerLatestReviewRequest`; `changes.rs` with `Snapshot`/`diff`/`diff_list`/`digest`; `state.seen` persisted; `•` + bold +
`+n` in table and sidebar; header `n unread`; the status-line digest (click to jump); `u` filter, `U` mark all, read-on-open;
own-action suppression; quiet first run seeded by `isReadByViewer`.

**Later:** detail-level `diff_detail`/`new_marks` with tab badges, `NEW` markers and `n`/`p`; Comfortable's second-row words;
timeline actors ("merged by alice") and the force-push banner; `[notify]` bell/desktop; `isReadByViewer` read-on-web sync; Actions
tab run completions feeding the same digest; a 1 s dwell before `J`/`K` marks read; "unread first" sort; comment ids for exact
marks after edits/deletions.
