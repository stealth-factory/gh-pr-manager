# Universal search (`/`) — design

Request: "a nice type to search experience that will offer fuzzy search across repo name, pr title, pr description, pr number etc",
then: "real just in time search while we type — anything above 3 characters or if we start typing in numbers", "the headline use
case is entering PR numbers", and "search should be universal, starts with the / key".
All GraphQL below was validated read-only with `gh api graphql` on 2026-10-19 (token scopes `repo, read:org`); timings are wall-clock
from this machine and **include ~0.5 s of `gh` CLI overhead**; cost is `rateLimit.cost`. Matching speed was measured with a release
build of a throwaway bench over 300 real PRs (`org:stealth-factory is:open`, bodies stripped and capped at 2000 chars, 1.9 KB average).

## 1. Interaction model: one palette, opened by `/` everywhere

**`/` is the single entry point.** It opens the search palette from the list screen, from either pane of the split detail view, and
over the Menu and Help modals (which it replaces). It stays a typed character wherever text is being edited: the comment composer and
the scope/repo pickers' filter inputs. Confirm dialogs ignore it (a yes/no question should not be navigated away from by accident).
Today's inline `/` substring filter is removed, not kept alongside: "filter the visible list" is what **Enter** in the palette does.
Mouse: a header chip ` ⌕ Search ` (after the repo chip, `Action::OpenSearch`), the footer chip `/ Search`, and the status-line chip
while a query is pinned. Free keys were checked (taken: `s S f / c m a R x y o r q ? D e 1-6 [ ] n p i J K g G h j k l Tab Space
. Enter Esc F5`); nothing else needs a global key — `Ctrl-o`/`Ctrl-g` below are bound only inside the palette.

**Universal = one input, results grouped by kind**, PRs first, each section with a header, headers never selectable:

| Section | Source | Picking a result |
|---|---|---|
| **Pull requests** | ranked loaded PRs (§3), then live GitHub results (§5) under a `From GitHub` divider, number hits (§6) | opens the PR in the detail view; the current layout is kept (list → detail, detail stays detail with the sidebar narrowed to the pinned query) |
| **Repositories** | `repo_items(app, text)` minus headers: in-view (with counts) → recent → your repos (fuzzy on `owner/name`) | applies the repo filter (`SetRepo`); the query is not pinned |
| **Scopes** | `scope_matches` (fuzzy on name + qualifier) | switches scope (same as the scope modal pick) |
| **Commands** | static table of existing `Action`s: Comment, Merge…, Approve, Request changes, Close/Reopen, Open in browser, Copy checkout, Copy URL, Refresh, Density, State: Open/Closed/Merged/All, Only this repo, Help, Quit | runs it on the selected PR (disabled ones show the same reason text the Actions menu shows) |

v1 ships all four — Repos/Scopes/Commands are in-memory lists and ~60 lines on top of the PR search. Later: authors and labels as
pickable entities ("filter to author"), the repo picker's remote tier C, files/comments inside a PR. Non-PR sections are shown only
when the query is ≥ 2 chars and they have a match with a nucleo score ≥ 60% of the best PR score, capped at 3 rows each, so they
never crowd out PRs; the number mode (§6) hides them entirely.

**Enter versus Esc.** `Enter` on a PR row = select it and open the detail view; the query is also **pinned** as the list filter
(`search.pinned`), so Esc/`q` from the detail view returns to the narrowed, ranked list and the status line shows the chip
` / form atlas skin  12 of 300 · ✕ `. `Enter` with the highlight on nothing (empty input) just closes. `Esc`/click outside closes
and discards what was typed since opening, leaving any previously pinned query alone. On the list screen `Esc` clears the pinned
query (today's `ClearFilter`), as does the chip's `✕`; clicking the chip text reopens the palette pre-filled. Scope/state/repo
changes keep the pinned query and re-rank when the new list arrives (`pending_select` keeps the selection by id).

| Key | Action |
|---|---|
| printable, Backspace, paste | edit (append/pop like the scope/repo modals); `Ctrl-u` clear, `Ctrl-w` delete word |
| ↑ ↓, Ctrl-p/n, PgUp/PgDn, wheel | move the highlight across sections |
| Enter / click | pick (PR → open; repo → filter; scope → switch; command → run); on a PR also pins the query |
| Ctrl-f | pin the query as the list filter **without** opening anything (the old `/` behaviour) |
| Tab | complete the current token from loaded values (`repo:` → repos in view, `author:`, `label:`, `is:`/`ci:`/`review:` enums) |
| Ctrl-g | force a GitHub search now (also the `↗` row) |
| Esc, click outside | close, discard typed text |

History: `state.recent_searches` (max 10, deduped, pushed on pick/pin) is listed under `Recent searches` while the input is empty;
picking one fills the input and runs it. Double-click a PR row = same as Enter. Footer in the palette:
`Enter Open · ^F Filter list · ^G GitHub · Esc Close`.

## 2. Fields and query syntax

Whitespace-separated tokens, `"quoted phrases"` kept together. Every token must match (AND). A token is a **free term**, a **number**
(§6) or a **prefix filter**; unknown prefixes (`foo:bar`) are plain text so titles containing colons still match.

| Token | Local match | Remote qualifier |
|---|---|---|
| `fix typo` | fuzzy over title, repo, branch, author, labels; substring over body | `fix typo in:title,body` |
| `#123` / `123` | number mode (§6): exact, then prefix | aliased `pullRequest(number:)` lookups |
| `repo:atlas` / `-repo:` | fuzzy on `owner/name` | `repo:owner/name` (resolved from the loaded list, else the literal) |
| `author:wii` / `author:@me` / `-author:` | fuzzy on login (`@me` → viewer) | `author:login` |
| `label:deps` / `-label:` | substring on any label name | `label:deps` |
| `head:feat` (alias `branch:`) | fuzzy on `headRefName` | `head:feat` (valid, but 4.5–5 s) |
| `is:draft` `is:open` `is:closed` `is:merged` | flags/state of loaded rows | `draft:true`, `is:…` |
| `ci:failing` `ci:passing` `ci:pending` `ci:none` | `ci_state()` | `status:failure/success/pending` |
| `review:approved` `review:changes` `review:required` `review:none` | `review_decision` | `review:approved/changes_requested/required/none` |

Prefixes are hinted, not learned: the palette's bottom line lists them dimly, `Tab` completes values, and a filter token that
matches nothing renders red in the input (`is:foo`). All validated remotely: `in:title,body`, quoted phrases, `-label:`, `draft:true`,
`review:approved`, `status:failure`, `head:`, multi-term AND (`fuzzy search` → 2 hits vs `search` → 10).

## 3. Local scoring

**Crate: `nucleo-matcher = "0.3.1"`** (helix-editor/nucleo, MPL-2.0, published 2024-02-20, 2.0 M downloads in the last 90 days,
the matcher behind Helix). Use `default-features = false, features = ["unicode-casefold", "unicode-normalization"]` so match
indices are plain `char` indices that line up with `title.chars()` (the default `unicode-segmentation` feature indexes grapheme
clusters). Rejected: `fuzzy-matcher 0.3.7` (skim v2) — last release 2020-10-04, 10× slower on titles in the bench (0.3–0.6 ms vs
0.01–0.12 ms for 300 titles) and treats the space in `swr 88` literally (0 hits); hand-rolled — nucleo already gives fzf-v2 quality,
case folding, normalisation and indices in ~20 lines of glue.

**Per term, max over fields; per PR, sum over terms; every term must hit something.**

| Field | Matcher | Weight |
|---|---|---|
| number exact / prefix | equality / `starts_with` | +100 000 / +50 000 |
| title | nucleo `AtomKind::Fuzzy` | ×10 |
| repo `owner/name` | fuzzy | ×9 |
| head branch · author | fuzzy | ×7 |
| labels (joined by `" "`) | fuzzy | ×6 |
| body (stripped, capped) | **substring** on a pre-lowercased copy, `score = len(term)×16×5`; terms < 3 chars skip the body | ×5 |

Fuzzy on bodies is deliberately excluded: fuzzy over 2 KB bodies matched 276–300 of 300 PRs for every realistic query
(`ambient occl` → 294), i.e. noise, at 3–26 ms. Substring gives precise hits and exact snippet ranges. Phrase tokens are substring
everywhere. Tie-breakers: score, then `updated_at` desc, then original index. nucleo scores are `u16`; sum in `u32`. Filters gate,
they don't score.

**Performance (measured, release build, 300 PRs, 1.9 KB avg bodies, 1–3 terms):** nucleo on the five short fields + `str::find`
on the lowercased body + indices for the 12 rows on screen: **0.18–1.3 ms per keystroke** (`fix` 0.19, `swr 88` 0.5, `form atlas
skin` 1.27 ms). nucleo's `AtomKind::Substring` on bodies instead: 1.7–5.9 ms — hence `str::find`. So: no local debounce, recompute
synchronously on each key, but **cache**: `visible()` is called several times per frame, so results live in `app.search.results`
and are recomputed only when `(prs_rev, text)` changes. Indices are computed only for drawn rows. `Utf32String` haystacks and the
lowercased body are built once per PR on arrival.

## 4. Data: labels and branch eager, bodies lazy

Measured on `is:pr archived:false is:open org:stealth-factory sort:updated-desc`, 50 nodes, 3 interleaved runs each, cost 1 always:

| Query variant | Latency | Payload |
|---|---|---|
| today's `SEARCH_QUERY` | 3.7 / 3.8 / 4.0 s | 24.6 KB |
| + `headRefName labels(first:10){nodes{name}}` | 3.7 / 3.4 / 5.0 s | 29.7 KB (+5 KB) |
| + `body` as well | 5.6 / 4.0 / 4.9 s | 445 KB (**18×**) |
| + `bodyText` instead | 6.9 / 5.4 / 4.3 s | 247 KB (smaller, slower: server-side render) |
| `author:@me` (43 PRs): base vs + body | 4.3 / 3.6 s vs 4.5 / 4.6 s | 20.9 KB vs 198 KB |
| `nodes(ids:[50]){… on PullRequest{id updatedAt body}}` | 1.06 / 1.22 / 1.32 s | 421 KB |
| `nodes(ids:[100])` | 1.8 / 1.9 / 2.1 s | 1.25 MB |

Bodies on this org: raw mean 8.1 KB, median 4.0 KB, p90 10.3 KB, max 64 KB (GitHub's cap; 48 of 50 exceed 1 KB). Stripped (HTML
comments, fenced code, tags, image/link targets, URLs, markdown punctuation runs, whitespace collapse) mean 4.3 KB; capped at 2000
chars 1.9 KB — **300 PRs ≈ 570 KB resident** (≈ 1.2 MB with the lowercase copy).

- **Eager**: add `headRefName` and `labels(first:10){nodes{name}}` to `SEARCH_QUERY` (+5 KB/page, no measurable latency).
- **Lazy**: bodies via `BODIES_QUERY` (`nodes(ids:)`, chunks of 100, 1 point, ~2 s) spawned after every `Msg::Prs` page — not on
  first `/`, so the first search is already complete. Keeps the first paint ~1 s faster and 18× lighter; a 120 s auto-refresh would
  otherwise re-download 2.5 MB per 300-PR scope.
- **Cache by `(id, updatedAt)`**: a body edit bumps `updatedAt`, so refreshes refetch only changed PRs (steady state: 0 extra
  points). Strip and cap on arrival, drop the raw string. Skip when `rate.remaining < 300`; cap at 1000 entries, evicting ids no
  longer in `prs`. Config `[search] bodies = false` disables body fetching (hint shows `descriptions off`).

## 5. Live remote search while typing

**Verdict: yes, with three constraints.** (1) Local results are always instant and first; the remote part only ever *adds* rows
below a divider, so nothing waits on the network. (2) GitHub search is whole-word: it does **not** do prefix matching, so firing on
every keystroke mid-word buys nothing — measured `shoul` → 0 results in every scope, `shoul*` → 0 (no wildcards), `shoulder` → 3.
Remote queries are therefore built from **complete words** and re-ranked locally with the full text. (3) A self-imposed budget and
backoff, because the limits are generous but "subject to change without notice".

**Latency, within scope qualifiers (30 nodes, title/number/repo only; subtract ~0.5 s `gh` overhead for in-app numbers):**

| Query | `involves:@me` | `org:stealth-factory` | All PRs (`user:` + 9 `org:`) | `repo:…/form-atlas` |
|---|---|---|---|---|
| `ref in:title,body` | 1.21 s · 12 | 1.61 s · 47 | 1.67 s · 49 | 0.71 s · 0 |
| `shoul` / `shoul*` | 0.85 s · 0 | 0.83 s · 0 | 0.93 s · 0 | 0.63 s · 0 |
| `shoulder` | 0.88 s · 3 | 0.99 s · 3 | 0.91 s · 3 | 0.83 s · 3 |
| `#14` (text) | 1.64 s · 19 (junk) | 1.55 s · 121 (junk) | 1.51 s · 136 (junk) | 0.86 s · 1 (wrong PR) |
| `142` (text) | 0.80 s · 0 | 1.10 s · 6 (junk) | 1.00 s · 6 (junk) | 0.62 s · 0 |

So 0.3–1.2 s in-app, cost 1 each; the multi-org "All PRs" scope is no slower than one org. Number-as-text is junk (§6).

**Rate limits (docs.github.com, checked 2026-10-19).** GraphQL search counts against the `graphql` bucket (5000 points/h; a search
page costs 1) — the 30 requests/min limit is the REST `search` bucket (`gh api rate_limit` lists both) and does not apply here.
Secondary limits: **2,000 points/min for the GraphQL endpoint (non-mutation request = 1 point)**, 100 concurrent requests, 90 s CPU
per 60 s (60 s of it for GraphQL); on 403/429 honour `retry-after`, else wait ≥ 60 s. Design:
- **Trigger**: 400 ms idle after the last keystroke (ride the 200 ms tick like the repo picker), text ≥ 3 chars (the user said "above
  3"; `ref` is a useful 3-char word, so ≥ 3), and at least one *complete* free term (followed by whitespace, or the whole input when
  idle) or a prefix filter. The query sent is `"<complete terms> in:title,body <mapped qualifiers> <search_query() minus sort>
  sort:updated-desc"`, `first:30`, through the existing `search_page`. Enter/Ctrl-g treat the last token as complete.
- **Budget**: a token bucket of 20 remote searches/min (refill 1 per 3 s, burst 20) — far below 2,000/min but enough for a fast
  typist; when empty the `↗` row reads `⏳ GitHub search paused (n s)`. `rate.remaining < 100` disables remote entirely
  (`rate limited — local results only`). A 403/429 pauses for `retry-after` (default 60 s) with the same visible hint.
- **Staleness**: `remote_gen` bumps per dispatch; a response with an older generation, a different text, or arriving after the
  palette closed is dropped (reqwest futures are not cancelled — one in-flight request at a time, the next waits for the tick).
- **Cache**: `remote_cache: HashMap<String, Vec<PrSummary>>` keyed by the exact remote query (cap 30, cleared on scope/state/repo
  change, entries older than 5 min refetched), so backspacing and retyping are free.
- **Partial-word compensation**: the remote results (full `PrSummary`s) are re-ranked by the local scorer with the *full* text,
  including the partial last token, over title/repo/author/branch/labels; a term that fails locally does not drop a remote row
  (GitHub matched it in the body we don't have yet) but ranks it last. Their bodies are queued through the lazy loader, so snippets
  and body highlights appear a second later. Rows already in `prs` are deduplicated by id.
- **Merging and presentation**: local rows render instantly; when the remote query is in flight the `↗` row becomes
  `⠋ searching GitHub for “shoulder” in org: stealth-factory · Open…`; results stream in under `From GitHub · N more`
  (dim `↗` marker). Picking one splices it to the front of `prs` (bump `prs_rev`), pins and opens it; it disappears on the next
  list reload like any row. Errors: dim red `GitHub search failed: <msg>`, local rows stay.
- **Kill-switch**: `[search] live = true` in config (`false` → remote only on Ctrl-g/Enter-with-no-local-hits; the `↗` row stays).

## 6. Number mode — the headline flow

Input that starts with a digit or `#` switches to number mode from the first character: other sections hide, the input shows a
dim `#` prefix, and the hint reads `type a PR number · Enter opens an exact match`. `#14`, `14`, `14 ` all mean the same.

- **Instant local**: exact number matches from `prs` first, then prefix matches (`14` → #14, #140–#149, #1400…), exact ranked
  first, then by `updated_at`; each row shows its repo so #142 in form-atlas and #142 in opscope are distinguishable.
- **Remote, exact only** (GitHub cannot prefix-match numbers): one request with **aliased `repository(owner,name){pullRequest(number:N)}`
  lookups** across candidate repos — the active repo filter alone if set; otherwise repos in the current list (by PR count) → recent
  repos, **capped at 12 aliases**. Measured (full `SEARCH_QUERY` node fields per alias, cost **1 for the whole batch**):
  10 aliases 0.85–2.5 s, 20 aliases 1.7–3.7 s (≈ 0.1–0.15 s per alias); a single alias 0.7 s. Missing numbers come back as
  `NOT_FOUND` entries in `errors[]` **with partial `data`**, so this needs `Client::graphql_partial` that keeps `data` and only fails
  on non-`NOT_FOUND` errors (today's `graphql()` bails on any error). Ambiguity is real: #14 exists in 13 of 20 repos here, #142 in
  5 — always show the repo, state icon and age; rank by the candidate order (active repo → in-view count → recent).
  Text search post-filtered by number was tested and rejected: `142` returned 200 hits and #142 appeared only by luck, `14` returned
  1020 hits without PR #14 in the first 30; without a repo context the `↗ Search GitHub` row is offered instead of guessing.
- **Speed**: debounce 150 ms (cheap, precise); each digit fires a fresh exact lookup for the number typed so far (`1`, `14`, `142`),
  stale generations dropped; cache per `(repo, number)` (hits and misses, 10 min) so backspacing is free. Counts against the same
  20/min bucket. Bodies for hits go through the lazy loader.
- **Enter on an exact, unique match** (one row whose number equals the typed number, local or remote) opens it directly; with several
  exact hits in different repos the highlight sits on the first and Enter picks the highlighted one. `#14 ` + Enter on a 14-less
  list with lookups still in flight shows `⠋ looking up #14 in 12 repositories…` and opens when the single hit lands.

## 7. UI

Palette (width 96; `‹›` marks highlighted spans — accent + bold; owner dim, name bold, number dim, author/age dim right):

```
┌ Search ─────────────────────────────────────────────────────────────────────────────────┐
│ › form atlas skin▏                                                   12 of 300 · ⠋ 150/300 │
│ ─────────────────────────────────────────────────────────────────────────────────────────│
│ Pull requests                                                                             │
│▌● stealth-factory/‹form›-‹atlas›  #151  SWR-899: app ‹skin› deforms like a human s…  wiiiimm · 3h│
│     …the ‹skin› mesh follows the ‹atlas› rig; see the ‹form› contract in ADR-7 before…    │
│ ○ stealth-factory/‹form›-‹atlas›  #153  SWR-878: surface mapping contract — ADR-7    wiiiimm · 5h│
│     …‹skin› weights are recomputed per pose from the ‹atlas› bind…                        │
│ From GitHub · 3 more                                                                      │
│ ↗ stealth-factory/form-atlas  #41  feat(rig): skin binding for the atlas       wiiiimm · Sep 2│
│ ↗ Search GitHub for “form atlas skin” in org: stealth-factory · Open · all repos    Ctrl-g│
│ Repositories                                                                              │
│   stealth-factory/‹form›-‹atlas›                                           10 PRs · filter│
│ ─────────────────────────────────────────────────────────────────────────────────────────│
│ ↑↓ move · Enter open · ^F filter list · Tab complete · ^G GitHub · Esc close             │
│ repo:  author:  label:  head:  #123  is:draft  ci:failing  review:approved               │
└──────────────────────────────────────────────────────────────────────────────────────────┘
```
PR row = `state icon · repo · #number · title (truncated) · author · age`; the snippet line appears only when a term hit the body
(60 chars around the first hit, word boundaries, `…` both sides, all hits highlighted) and only when the palette is ≥ 60 cols.
`⠋ 150/300` at the input's right edge while bodies load (results re-rank as they arrive). Commands render as
`▸ Squash and merge  #151` / dim `▸ Approve — can't review your own PR`; scopes as `◉ Review requested  review-requested:@me`.

```
 › #14▏                                                     type a PR number · Enter opens an exact match
 ─────────────────────────────────────────────────────────────────────────────────────────
 Pull requests
▌✔ stealth-factory/form-atlas         #‹14›   feat(rig): prepare sourced leg joints      wiiiimm · Mar 3
 ● stealth-factory/tldrrr-monorepo    #‹14›2  feat(api): summaries cache                 wiiiimm · 2h
 ● stealth-factory/opscope            #‹14›0  fix(core): tmux pixel charts               wiiiimm · 1d
 From GitHub · ⠋ looking up #14 in 12 repositories…
 ↗ stealth-factory/opscope            #14    chore: initial scaffold  (merged)          wiiiimm · Jan 9

 › zzqx▏                                                                       0 of 300
 ─────────────────────────────────────────────────────────────────────────────────────────
   No loaded PRs match “zzqx”
   ⠋ searching GitHub for “zzqx” in org: stealth-factory · Open…           (Esc closes)

 › shoul▏                                                                      2 of 300
 ─────────────────────────────────────────────────────────────────────────────────────────
 Pull requests
▌● stealth-factory/form-atlas  #150  SWR-898: R5 ‹shoul›der girdle in the app           wiiiimm · 4h
 ↗ Search GitHub for “shoul” — GitHub matches whole words; finish the word or press Ctrl-g
```
Empty input: `Recent searches` + the prefix hint. Pinned state outside the palette: status line ` / form atlas skin   12 of 300 ·
click to edit · Esc clears  ✕ `; table/sidebar rows carry the same highlight spans in the repo, `#`, title and author cells; a
body-only match appends dim ` · …snippet…` to the title cell when the column has room. Header gains ` ⌕ Search ` (clickable).
Help: `/  Search everything: PRs (title, description, #, repo, author, branch, labels), repos, scopes, commands`.

## 8. Rust-level additions

```rust
// Cargo.toml
nucleo-matcher = { version = "0.3.1", default-features = false, features = ["unicode-casefold", "unicode-normalization"] }

// config.rs
#[serde(default)] pub search: SearchConfig,   // pub struct SearchConfig { pub live: bool /*true*/, pub bodies: bool /*true*/ }

// github.rs — SEARCH_QUERY gains `headRefName labels(first:10) { nodes { name } }`; the node fields move into a `PR_FIELDS` const
#[serde(default)] pub head_ref_name: String,     // on PrSummary
#[serde(default)] pub labels: Nodes<LabelName>,  // pub struct LabelName { pub name: String }
pub struct PrBody { pub id: String, pub updated_at: String, pub body: String }
const BODIES_QUERY: &str = "query($ids:[ID!]!){ rateLimit{remaining limit} nodes(ids:$ids){ ... on PullRequest { id updatedAt body } } }";
impl Client {
    /// Like `graphql`, but keeps `data` when every error is `type: NOT_FOUND`.
    async fn graphql_partial<T>(&self, query: &str, vars: Value) -> Result<(T, Option<RateLimit>)>;
    pub async fn pr_bodies(&self, ids: &[String]) -> Result<(Vec<PrBody>, Option<RateLimit>)>;             // ≤ 100 ids, null nodes skipped
    /// One request: `r0: repository(owner:,name:){ nameWithOwner pullRequest(number:$n){ PR_FIELDS } } r1: …` (≤ 12 aliases).
    pub async fn prs_by_number(&self, repos: &[String], n: u64) -> Result<(Vec<PrSummary>, Option<RateLimit>)>;
}

// search.rs (new) — pure, no App access, fully unit-testable
pub enum Field { Number, Title, Repo, Branch, Author, Labels, Body }
pub enum Token { Term(Atom, String /*lowercase*/), Phrase(String), Number(u64), Repo(String, bool /*neg*/), Author(String, bool),
                 Label(String, bool), Head(String), Is(IsFlag, bool), Ci(CiFlag), Review(ReviewFlag), Invalid(String) }
pub struct Query { pub tokens: Vec<Token>, pub raw: String, pub number_mode: bool, pub complete: usize /*tokens followed by whitespace*/ }
pub fn parse(raw: &str, me: Option<&str>) -> Query;
pub fn remote_query(q: &Query, base: &str /*search_query() minus sort*/, all_tokens: bool) -> Option<String>;  // None: nothing searchable
pub fn strip_body(raw: &str, cap: usize) -> String;
pub struct Doc { title: Utf32String, repo: Utf32String, branch: Utf32String, author: Utf32String, labels: Utf32String,
                 number: u64, number_str: String, body: Option<(String /*display*/, String /*lowercase*/)>, updated: i64 }
impl Doc { pub fn new(pr: &PrSummary, body: Option<&str>) -> Self; pub fn set_body(&mut self, stripped: &str); }
pub struct Match { pub score: u32, pub field: Field, pub body_hit: Option<(usize, usize)>, pub exact_number: bool }
pub fn score(m: &mut Matcher, q: &Query, d: &Doc, pr: &PrSummary, lenient: bool /*remote rows*/) -> Option<Match>;
pub fn rank(m: &mut Matcher, q: &Query, docs: &[Doc], prs: &[PrSummary]) -> Vec<(usize, Match)>;
pub fn highlight(m: &mut Matcher, q: &Query, field: Field, d: &Doc) -> Vec<u32>;      // per drawn row only
pub fn snippet(d: &Doc, q: &Query, width: usize) -> Option<(String, Vec<(usize, usize)>)>;
pub fn complete(q: &Query, values: &Values) -> Vec<String>;                            // Tab; Values = repos/authors/labels in view
pub fn commands(q: &Query, m: &mut Matcher) -> Vec<(u16, Command)>;                    // static table → scored

// app.rs
pub enum Item { Header(&'static str), Pr { idx: usize, m: Match }, Remote { i: usize, m: Option<Match> }, Repo(String),
                Scope(usize), Command(Command), Recent(String), SearchGitHub, Note(String) }   // Header/Note not selectable
pub struct Search {                     // replaces `filter` / `filter_input`
    pub text: String, pub pinned: Option<String>, pub sel: usize, query: Query,
    matcher: Matcher, docs: Vec<Doc>,                       // parallel to prs; rebuilt on first page, bodies patched in place
    pub results: Vec<(usize, Match)>, results_for: (u64, String),
    pub bodies: HashMap<String, (String /*updatedAt*/, String /*stripped*/)>, bodies_loading: HashSet<String>,
    pub remote: Option<Remote>,         // { query: String, hits: Vec<PrSummary>, loading: bool, error: Option<String>, kind: Text|Number }
    remote_cache: HashMap<String, (Instant, Vec<PrSummary>)>, number_cache: HashMap<(String, u64), Option<PrSummary>>,
    remote_gen: u64, typed_at: Instant, bucket: TokenBucket /*20/min*/, paused_until: Option<Instant>,
}
pub search: Search, prs_rev: u64,       // += 1 on Msg::Prs, Msg::Bodies, remote splice, Msg::Detail title sync
Modal::Search                           // state lives in app.search so pinned/unpinned share one struct
Action::OpenSearch, Action::PickSearch(usize), Action::PinSearch, Action::ClearSearch, Action::SearchGitHub
Msg::Bodies { res: Result<(Vec<PrBody>, Option<RateLimit>)> }
Msg::Remote { generation: u64, query: String, res: Result<(Vec<PrSummary>, Option<RateLimit>)> }   // text search and number lookups
pub fn search_items(app: &App) -> Vec<Item>   // sections in §1 order; cheap: results cached, non-PR lists ≤ ~1000 strings
// state.rs
pub recent_searches: Vec<String>,       // max 10, newest first, deduped case-insensitively
```
`visible()`: pinned → cached `search.results` indices (`ensure_results()` recomputes when `(prs_rev, text)` differs), else
`0..prs.len()`. `Msg::Prs` first page rebuilds `docs` and re-attaches cached bodies; every page queues bodies whose
`(id, updated_at)` is unknown in chunks of 100 (when `cfg.search.bodies`). `on_tick` drives both debounces (400 ms text, 150 ms
number) and the bucket/pause. `on_key`: `/` → `OpenSearch` unless a Composer/Scopes/Repos modal is open; Menu/Help are replaced.
`on_wheel`/`on_click` reuse `hits.modal_items` (a PR row and its snippet line map to the same index). `Paste` appends to
`search.text`. Scope/state/repo changes clear `remote_cache`, keep `pinned`.

Tests (`search.rs`, `app.rs`): parse (`"quoted phrase"`, `-label:x`, `author:@me` → login, `#14`/`14`/`14 ` → number mode, unknown
prefix stays text, `is:foo` → `Invalid`, `complete` count); AND semantics; field precedence (`#439` exact beats a title containing
439; title beats repo beats body); number ranking (exact before prefix, then recency); tie-break by `updated_at`; `strip_body`
removes comments/fences/tags/URLs and cuts at a char boundary; `snippet` window and ranges; `remote_query` composition, complete-
words-only, and `None` for number-only; lenient scoring keeps remote rows; stale `Msg::Remote` generations dropped; token bucket
refill and pause; `graphql_partial` keeps data with `NOT_FOUND` errors and fails on others; `recent_searches` dedupe/truncate;
`visible()` cache invalidation on `prs_rev`; a 300-PR fixture asserts `rank` < 5 ms in debug.

## Implementation notes

Deviations from the design above, as built (2026-10-19), adapted to the Actions tab (tabs `1`–`7`; it owns `O H E W z Z t` and
`Ctrl+F` while active) and inline images (`I`, `Modal::Image`), which came after the design:

* **Keys.** `/` opens the palette from the list, both detail panes and the Actions tab, and over Help, the Actions menu and the
  image viewer. It stays text in the composer, the scope/repo pickers and the Actions log-search input (`on_log_search_key` runs
  before the key map); Confirm ignores it. The palette's `Ctrl-f` (pin) can't meet the Actions tab's `Ctrl+F` (log search):
  `on_key` hands every key to `on_modal_key` while a modal is open, and the Actions binding only applies with none open.
  `Ctrl-Backspace`/`Alt-Backspace` delete a word like `Ctrl-w`. `/` and the header chip open an empty input; clicking the
  pinned chip in the status line opens it pre-filled (`Action::EditSearch`). The empty input lists recent searches, but Enter
  on it still just closes unless an arrow/wheel moved the highlight.
* **Number tokens.** Number mode needs the input to be a single number (`#14`, `14`, `14 `). A number elsewhere in a query
  (`fix 439`) scores exact/prefix on the number and otherwise falls back to fuzzy text, which is what makes `#439` beat a
  title containing 439.
* **Fuzzy floor.** Real titles showed nucleo matching letters scattered across long titles ("atlas" in "summaries **at**
  three **l**engths **a**nd… with pull-quote**s**" = 98, "skin" = 64). A field hit for a fuzzy term of ≥ 3 chars needs
  `20·len + 4` (contiguous words score ≈ 28/char, abbreviations like `ghprm` → gh-pr-manager 125, `frmatls` → form-atlas 149
  pass). The same floor applies to repos, scopes and commands. Highlights mark every literal occurrence of a term (nucleo only
  reports its best one, often past the truncation), falling back to the fuzzy positions for abbreviations.
* **Palette size.** At most 10 loaded PRs are listed, then a selectable `… N more loaded matches` row (Enter = `Ctrl-f`), so
  GitHub rows and the other sections stay reachable. The non-PR threshold is 60% of the best PR score divided by the title
  weight (10). The palette is 96 wide, anchored near the top so it grows downwards; Compact drops the rules and snippet lines,
  Comfortable adds a blank line between sections. Section headers are clickable (move the highlight), and so are the filter
  hints on the bottom line (they insert the token).
* **Remote timing.** Debounces ride the 200 ms tick, so text searches fire 400–600 ms after the last key and number lookups
  150–350 ms. Once idle, the whole input counts as complete (so a paused `shoulder` searches without a trailing space); while
  still typing a partial word the `↗` row shows the whole-words hint. A 403/429 pauses for 60 s (`graphql()` doesn't surface
  `retry-after`). Responses that arrive stale still fill `remote_cache`/`number_cache`; only their display is dropped. The
  list's auto-refresh pauses while the palette is open, like it does for the composer. `[search] live = false` also stops
  number lookups until Enter / `Ctrl-g`.
* **Number lookups.** When the lookup lands and the highlight (untouched) isn't on an exact hit, it moves to the first exact
  one. Enter before it lands opens the single exact hit, highlights the first of several, or toasts when there's none. With no
  candidate repo at all the palette says to pick one (`S`) rather than offering a junk text search.
* **GitHub rows.** One opened from the palette is spliced into `prs` and kept across a list reload while it's selected in the
  detail view (otherwise the 60 s refresh would yank it out from under the reader).
* **Commands.** Added `Show / hide images` (`I`); `State: …` uses a new `Action::SetState`, `Copy PR URL` a new
  `Action::CopyUrl` (the menu uses it too). Disabled reasons come from the Actions menu's own `menu_entries()`.
* **API shape.** `remote_query` also takes the loaded repo names (to resolve `repo:atlas`). `search_page` gained a `first`
  argument (`SEARCH_QUERY` takes `$first`); the row fields are a `pr_fields!()` macro so `SEARCH_QUERY` stays a `concat!`
  const. `Token` has a `Pending` variant (`repo:` with no value, ignored); `Invalid` tokens (`is:foo`) are ignored rather than
  failing every row. Clicks go through `ModalPick` like the other pickers, so there is no `Action::PickSearch`.
* **Bodies.** `strip_body` keeps a lone `-`/`_` inside a word (`snake_case`, `wp-stateless`).
* **Performance.** `nucleo-matcher` builds with `opt-level = 3` in the dev profile too, so the 300-PR rank test holds its 5 ms
  budget in debug (measured 0.9–2.4 ms debug, 0.02–0.37 ms release). A full 160×50 frame per keystroke (rank + palette rows
  + highlights + the table underneath, 300 PRs with descriptions, 1000 repos) takes 2.7–3.1 ms in release, about 1 ms of it
  the table; the first keystroke after the list changes also rebuilds the docs (≈ 7 ms).
