# Repository filter — design

Request: "add a new scope filter for repo … see a list of repos and pick / filter search … easily scope PRs down to PRs for a repo."
All GraphQL below was validated read-only with `gh api graphql` on 2026-10-18 (token scopes `repo, read:org`); timings are wall-clock from this machine, cost is `rateLimit.cost`.

## 1. Model: a combinable repo filter, not a scope

The repo is a **second axis layered on the scope**, exactly like the state filter: `My PRs` + `stealth-factory/opscope` gives
`is:pr archived:false is:open author:@me repo:stealth-factory/opscope sort:updated-desc` (validated: `involves:@me repo:X` returned the expected 1 PR, cost 1, 0.8 s).

Why not a scope: "scope down" means narrowing *what I already look at*; "my PRs in X", "review requested in X" are the common asks and need the scope to survive. A `repo:X` scope would throw the who-axis away, and users who want that can already add `[[scopes]] query = "repo:owner/name"` in config.
To still answer "every PR in this repo", the scope list gets one extra built-in, **`Everyone` (qualifier empty)**, shown and selectable only while a repo filter is set (validated: bare `repo:X` search works). Clearing the repo while `Everyone` is active resets the scope to index 0 with toast `Scope reset to My PRs` (an unbounded `is:pr is:open` query is useless).

Header (after the `◐ state` chip): ` ◫ All repos ▾ ` when unset; ` ◫ owner/name ✕ ` when set, blue (same as the Repository column), underlined like the other chips, name truncated to 28 cols. Two hit rects: the name → `Action::OpenRepos`, the `✕` → `Action::SetRepo(None)`. The status line already prints `search_query()`, so the `repo:` qualifier shows there for free. Count text becomes `12 PRs in owner/name` when set.

## 2. Where the repo list comes from

The picker must open instantly, so it never waits on the network. It merges three tiers (all in memory):

| Tier | Source | When | Cost / latency |
|---|---|---|---|
| A `In this view` | repos counted from the loaded `app.prs` (≤ 300 rows) — scope-and-state specific, with counts | recomputed on every `Msg::Prs` while `repo.is_none()` | free |
| B `Your repositories` | `viewer.repositories` all pages, no PR counts, PUSHED_AT desc, cap 500 | background, started in `start()`; persisted (§4) so it is complete on the next launch before the fetch finishes | 1 point + 1.1–1.8 s per 100 (viewer has 698 → 5 pages ≈ 7 s, 5 points) |
| C `More on GitHub` | `search(type:REPOSITORY)` restricted to the viewer + their orgs | only when ≥ 2 chars typed and < 5 local matches, 300 ms debounce | 1 point, 1.5–2 s |

**Per-repo open-PR counts are not worth it in tier B**: `openPullRequests: pullRequests(states:OPEN){totalCount}` keeps cost at 1 but raises page latency from 1.1–1.8 s to 3.3–4.3 s (measured 4 runs each), and the number is global, which misleads next to a scope like `My PRs`. Tier A's counts are the ones the user actually wants. Tier C keeps the field (20 nodes, measured 1.74 s) and shows it dim as `5 open`.
Org scopes need no extra query in v1: ORGANIZATION_MEMBER affiliation already brings the member-visible org repos into tier B, and tier C's `user:<org>` qualifier covers the rest. (`organization(login:){repositories(...)}` was validated too — 199 repos, cost 1, 3.8 s with counts — keep for later.)

Ranking with an empty filter: `All repositories` (✓ when unset) → `This PR's repo: owner/name` (only if a PR is selected and its repo ≠ active) → section A sorted by count desc, name asc, each `N PRs` (`N+` while `total > prs.len()`) → `Recent` (persisted picks, max 8, minus already-listed) → section B in PUSHED_AT order. With a filter: one flat list, case-insensitive substring over `owner/name`; rank = name starts-with > name contains > owner contains, ties keep tier order; `This PR's repo` and `Recent` entries get the same treatment. Substring, not fuzzy — consistent with the scope modal and `/`. If the text contains `/` and matches `^[\w.-]+/[\w.-]+$`, a last row `Use "owner/name"` applies it verbatim (no validation; an unknown repo simply loads 0 PRs). Tier C results that are not already present append under their header once they arrive.

Validated queries (exact strings for `github.rs`):

```graphql
# viewer_repos_page — cost 1, 1.1–1.8 s per page; page 2 via endCursor validated
query($after:String) {
  rateLimit { remaining limit }
  viewer { repositories(first:100, after:$after,
      affiliations:[OWNER,COLLABORATOR,ORGANIZATION_MEMBER],
      ownerAffiliations:[OWNER,COLLABORATOR,ORGANIZATION_MEMBER],
      orderBy:{field:PUSHED_AT, direction:DESC}, isArchived:false) {
    totalCount pageInfo { hasNextPage endCursor }
    nodes { nameWithOwner isPrivate isFork pushedAt } } }
}
# search_repos — cost 1, 1.5–2.0 s; $q = "<text> in:name user:<login> user:<org1> … archived:false sort:updated-desc"
# (`user:` accepts org logins; private repos are returned; "stealth" gave 3 clean hits vs 15 789 unqualified)
query($q:String!) {
  rateLimit { remaining limit }
  search(query:$q, type:REPOSITORY, first:20) { repositoryCount
    nodes { ... on Repository { nameWithOwner isPrivate pushedAt
                               openPullRequests: pullRequests(states:OPEN) { totalCount } } } }
}
```
If the typed text is `owner/name…`, build `$q` as `<name> in:name user:<owner>` instead, so non-affiliated owners can be found.

## 3. Picker UX

**Keys.** `S` (Shift+s) opens the picker everywhere a modal may open: `s` picks *who*, `S` picks *where*. Inside the scope modal `Tab` (or clicking the ` Repository › ` segment in its title) swaps to the repo picker and vice-versa, so anyone who knows `s` discovers it. Footer (list screen) gains `S Repo`; help gets `S / click repo chip  Filter to one repository` and `e  Toggle filter to the selected PR's repo`. `e` is the one-keystroke shortcut: press once → `SetRepo(Some(selected PR's repo))`, press again while that repo is active → `SetRepo(None)`; also offered in the Actions menu as `e  Only this repository` and as a click on the `owner/repo` line of the detail info pane (internal link `#repo:owner/name`, same mechanism as the existing `#file:` links). Free keys were checked: `e`, `S`, `t`, `w`, `d` (plain), `b`, `u`, `v`, `z` are unused.

**Layout.** `centered(area, min(72, w-4), min(rows+7, h-4))`, `modal_block("Choose repository")`, title bar right segment ` ‹ Scope (Tab) ` clickable. Row 1 filter input (`› type a repository…`), separator, scrolling list, hint line `↑↓ move · Enter / click select · Tab scope · Esc close`. Rows:

```
▌✓ All repositories                               36 PRs
   ⌖ This PR's repo   stealth-factory/opscope       5 PRs
   In this view
   stealth-factory/form-atlas                       10 PRs
   stealth-factory/opscope                           5 PRs
   Recent
   wiiiimm/hongkong-sandbox
   Your repositories                      (698 · updated 2m ago)
   stealth-factory/virtual-product-studio      🔒   pushed 3h
   …
   More on GitHub  ⠋ searching “opsc”…
```
Owner dim, name bold, active repo marked `✓` green (like the scope modal), count/pushed-ago right-aligned dim, `🔒` for private (optional). Section headers, loading/empty/error lines are not selectable: `sel` indexes only selectable rows and only those are pushed to `hits.modal_items`, so wheel/click/↑↓ behave exactly as in `Modal::Scopes` with no further changes to `on_wheel`/`on_click`.

**Picker keys**: printable/Backspace edit the filter and reset `sel = 0`; ↑/↓, Ctrl-p/n move; Enter picks; Esc closes; Tab → scope modal; paste appends (already handled for scopes in `on_event`). **Mouse**: click row = pick, click outside = dismiss, wheel = move selection, click `✕` chip in the header while the modal is closed = clear.

**Pre-selection on open**: the active repo if set, else `This PR's repo` if a PR is selected, else `All repositories`. So `S Enter` scopes to the selected PR's repo, and `S ↑ Enter` or `S Enter` on the top row clears.

**States**: tier B still loading → dim bottom line `⠋ loading your repositories… (200 of 698)`; remote in flight → `⠋ searching GitHub for “x”…` under the `More on GitHub` header; remote empty → `No repositories match “x”` (plus the `Use "owner/name"` row when applicable); remote error → dim red `GitHub search failed: <msg>` and local rows stay; `rate.remaining < 100` → `rate limited — local results only` and no remote call. Empty list screen when a repo filter is active: `No pull requests for <scope> in <repo>.` + `S picks another repository · click ✕ (or S, Enter) to show all repos`.

**Detail (split) view**: the modal overlays both panes like every other modal. Applying a repo does **not** change screen; `pending_select` (§5) keeps the selected PR if it survives the reload, otherwise the sidebar falls back to row 0 and the detail pane follows. Picking a *scope* keeps today's behaviour (jump to list).

## 4. Persistence

Add a machine-written state file next to the config but under XDG *state*: `$XDG_STATE_HOME/gh-pr-manager/state.json` (fallback `~/.local/state/gh-pr-manager/state.json`), written best-effort with `serde_json`, errors ignored:

```json
{ "recent_repos": ["stealth-factory/opscope", "…"],            // most recent first, max 10, deduped
  "repos_cache": { "login": "wiiiimm", "fetched_at": 1760745600,
                   "repos": [{ "nameWithOwner": "…", "isPrivate": true, "pushedAt": "…" }] } }
```
`recent_repos` is updated on every pick; `repos_cache` on fetch completion and used at startup only when `login` matches the viewer. Do **not** restore the last repo filter automatically (starting narrowed is surprising); instead add `default_repo = "owner/name"` (optional) to `Config`, applied like `default_scope` in `App::new`. `page_size`-style validation: ignore it with a toast if it does not look like `owner/name`.

## 5. Data and messages

```rust
// github.rs
#[derive(Debug, Clone, Deserialize, Serialize)] #[serde(rename_all = "camelCase")]
pub struct RepoInfo { pub name_with_owner: String, pub is_private: bool, pub pushed_at: String }
pub struct RepoPage { pub repos: Vec<RepoInfo>, pub total: usize, pub next: Option<String>, pub rate: Option<RateLimit> }
#[derive(Debug, Clone, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct RepoHit { pub name_with_owner: String, pub is_private: bool, pub open_pull_requests: Count }
impl Client {
    pub async fn viewer_repos_page(&self, after: Option<String>) -> Result<RepoPage>;          // VIEWER_REPOS_QUERY above
    pub async fn search_repos(&self, text: &str, owners: &[String]) -> Result<(Vec<RepoHit>, Option<RateLimit>)>; // REPO_SEARCH_QUERY above
}
// app.rs — App fields
pub repo: Option<String>,                 // active filter, API casing when known
pub repos: Vec<RepoInfo>,                 // tier B, PUSHED_AT desc
pub repos_total: usize, pub repos_loading: bool, pub repos_fetched: Option<Instant>,
pub recent_repos: Vec<String>,            // persisted
pub scope_repos: Vec<(String, usize)>,    // tier A, rebuilt on Msg::Prs while repo.is_none()
pub repo_search: Option<RepoSearch>,      // { text: String, results: Vec<RepoHit>, loading: bool, error: Option<String> }
repo_gen: u64, repo_search_gen: u64, pending_select: Option<String>,
// Modal / Msg / Action
Modal::Repos { filter: String, sel: usize, typed_at: Instant }
Msg::Repos { generation: u64, first: bool, res: Result<RepoPage> }
Msg::RepoSearch { generation: u64, text: String, res: Result<(Vec<RepoHit>, Option<RateLimit>)> }
Action::OpenRepos, Action::SetRepo(Option<String>), Action::ToggleRepoOfSelected   // S/chip/menu, pick/✕/#repo: link, e
// helpers
pub enum RepoItem { All { count: usize }, ThisPr(String, usize), Header(String), Repo { nwo: String, count: Option<usize>, open: Option<usize>, private: bool }, UseTyped(String), Note(String) }
impl RepoItem { fn selectable(&self) -> bool }        // Header/Note are skipped by sel and hits
pub fn repo_items(app: &App, filter: &str) -> Vec<RepoItem>   // §2 ranking; cheap enough per frame (≤ ~600 string compares)
fn repo_chip_label(&self) -> String
```
`search_query()` becomes `format!("is:pr archived:false {state} {scope} {repo} sort:updated-desc")` with `repo = self.repo.as_ref().map(|r| format!("repo:{r}")).unwrap_or_default()`, same whitespace collapse. `SetRepo` sets `self.repo`, pushes to `recent_repos` + saves state, sets `pending_select = selected id`, clears `prs`/`total`, `sel = 0`, `load_list()`; in `Msg::Prs` (first page) `prev = self.pending_select.take().or(selected id)`. The `Everyone` scope is appended at index 4 of the built-ins with `qualifier: ""` and filtered out of `scope_matches` while `repo.is_none()`.

**Debounce** rides on the existing 200 ms tick: in `on_tick`, if `Modal::Repos` is open, `filter.len() >= 2`, `typed_at.elapsed() >= 300 ms`, local selectable matches `< 5`, `repo_search.text != filter`, and `rate.remaining >= 100`, bump `repo_search_gen` and spawn `search_repos`. Results with a stale generation are dropped; `repo_search` is cleared when the modal closes.

**Caching**: tier B is fetched once per process (sequential pages, stop at 500 repos or when `remaining < 200`), merged with the persisted cache by `nameWithOwner` (fresh wins), then saved. If the picker opens and `repos_fetched` is older than 10 min, refetch page 1 only and prepend unseen repos (1 point). Tier A costs nothing and is always current for the unfiltered view; when the picker opens while a repo is active, the counts shown are from the last unfiltered load of the same scope+state (tag `scope_repos` with `(scope, state_filter)` and show the section only if it matches). Every response updates `self.rate`.

## 6. Edge cases

- **Scope changes while a repo is set**: the repo stays (orthogonal); list reloads with both; the empty-state hint names both axes. Picking `Everyone` requires a repo; clearing the repo under `Everyone` resets the scope (§1).
- **Repo with no PRs in the scope**: normal empty state; the picker still lists it (tier B has no count, tier A omits it).
- **Filter yields nothing**: only `All repositories` and, when the text looks like `owner/name`, `Use "owner/name"` are selectable; Enter on a non-selectable position is a no-op; the remote search fires as usual.
- **Rate limits**: background paging stops below 200 remaining, remote search below 100, both with a dim note; 403/429 on a page stops paging and toasts once; the picker keeps working from tiers A and the persisted cache.
- **Casing / renames**: `repo:` is case-insensitive; store the API's `nameWithOwner` when the pick came from tiers A–C, the typed text otherwise. A renamed repo may return 0 rows — the empty-state hint and `✕` cover it.
- **Capped list (300 rows)**: tier A counts get a `+` suffix while `total > prs.len()`, so they read as lower bounds.
- **Narrow terminals**: the header's right-hand group already hides itself when it exceeds half the width; the repo chip truncates at 28 cols and the modal shrinks to `w-4`.
