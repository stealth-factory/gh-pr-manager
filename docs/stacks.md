# Stacked pull requests — design

Request: "can we add support for stacked prs? and also expose the official gh stack feature that allow stacked pr to be previewed
and rebased etc", then "we can add a stacked pr view somehow". Everything below was validated read-only on 2026-10-19 with
`gh api` (account `wiiiimm`, scopes `repo read:org workflow gist`), `gh stack … --help`, `strings` on the installed gh-stack
binary, and docs.github.com; timings are wall-clock and include ~0.5 s of `gh` overhead; cost is `rateLimit.cost`. No mutating
call was made.

## 0. What exists on GitHub's side (findings that shape everything)

- **Native stacks are real and we have data to validate against.** `stealth-factory/opscope` has four stacks, all merged: #120 (2 PRs),
  #140 (5 PRs, merged one at a time over three hours), #163 and #196 (2 PRs each, merged atomically — same `merged_at` second).
  No *open* native stack is visible to the account (searched `author:@me`, `involves:@me`, every org, `user:wiiiimm`). Our own
  #1→#2→#3→#4 in this repo are an implicit chain (`stack` is null). Validated on #190/#194: `stack{number:196,size:2}`,
  `stackEntry{position}` 1 = bottom; `node(id:"PRS_…"){ ... on PullRequestStack { entries } }` lists merged entries too; after
  merging, every PR of #140 now has `base: main` (GitHub retargets upwards; the docs confirm "automatically re-target" and
  "automatically rebases the remaining branches" when the bottom merges).
- **GraphQL is read-only for stacks**: `PullRequest.stack { id number size baseRefName entries(first:) { totalCount nodes { id
  position pullRequest } } }`, `PullRequest.stackEntry { id position }`, timeline `AddedToStackEvent`/`RemovedFromStackEvent
  { stackNumber stackId actor }`, and `Ref.compare(headRef:) { aheadBy behindBy status }`. No `stack` mutation exists; `updatePullRequestBranch(pullRequestId,
  expectedHeadOid, updateMethod: MERGE|REBASE)` does.
- **REST does everything else, officially** (docs.github.com, API version `2026-03-10`; the list/get calls also answered under our
  `2022-11-28`): `GET /repos/{o}/{r}/stacks[?pull_request=N]` (0.66 s), `GET …/stacks/{n}`, `POST …/stacks {pull_requests:[bottom..top]}`
  (each base must equal the previous head; 422 otherwise or when stacks are not enabled for the repo), `POST …/stacks/{n}/add`,
  `POST …/stacks/{n}/unstack`. Merging: **`PUT …/pulls/{n}/merge-async {sha, merge_method, merge_action: default|direct_merge|merge_queue,
  bypass_rules}` → 202 `{status: pending|merged|enqueued|failed, details{uuid,error}}`, polled at `GET …/merge-async/{uuid}`**; the docs
  say it is *required* for stacked PRs and "includes all open downstack PRs", and that plain `PUT …/merge` "does not support stacked
  pull requests". `PUT …/pulls/{n}/update-branch {expected_head_sha}` is merge-only. The gh-stack binary uses exactly these paths.
- **gh-stack**: installed v0.1.0 (2026-07-29); latest v0.2.1 (2026-10-09; repo created 2026-02, 1.7k stars, 150 open issues, docs
  tagged "public preview"). Remote by number, no checkout: `merge <stack|pr>`, `unstack <stack>`, `link` (PR numbers only; branch
  names get pushed). Need a local tracked stack: `view` (`--json` has no documented schema), `rebase`, `sync`, `submit`, `push`,
  `modify`, `up/down/top/bottom/trunk`. Verified in a temp clone: `view --json` outside a tracked stack → `✗ current branch "main"
  is not part of a stack` (exit 2); `checkout 3` → `✗ PR #3 is not part of a stack on GitHub`. There is no `--repo` flag: the cwd
  must be a clone of the repo. Exit codes: 2 not a stack, 3 rebase conflict, 4 API failure, 8 locked, 9 stacks not enabled.
  So **`gh stack view --json` is useless to us remotely**; our own queries carry more than it prints.
- Cross-fork stacks are unsupported natively ("all branches in the same repository"); implicit chains are same-repo by construction.

## 1. Detection

**Native.** `pr_fields!` gains `baseRefName stack { id number size baseRefName } stackEntry { position }`. Measured on `org:stealth-factory
is:open`, 50 nodes, interleaved: cost **1 → 1**, 5.8–7.1 s vs 6.3–7.1 s (noise). Nesting `entries(first:20)` in the list costs **2** —
rejected; at `first:100` even today's query costs 2 and that org scope returns all-null nodes (`RESOURCE_LIMITS_EXCEEDED`), so the
page size stays 50. `DETAIL_QUERY` gains `baseRefOid`, `repository { defaultBranchRef { name } }` and `stack { … entries(first:100) {
totalCount nodes { position pullRequest { id number title state isDraft headRefName baseRefName headRefOid mergeable mergeStateStatus
reviewDecision commits(last:1){nodes{commit{statusCheckRollup{state}}}} } } } }`: cost **2 → 2**, 1.8–2.0 s. The detail is the source of
truth for the open PR's stack (members beyond the loaded list included); the list fields give every row its chip. Only details
fetched since the list started loading override it, newest last, so a stack changed or dissolved elsewhere isn't kept alive.

**Implicit** (`stacks::chains`): among loaded `PrSummary`s of one repo, `parent(p)` = the PR whose `headRefName == p.baseRefName`,
preferring OPEN over MERGED over CLOSED, newest number on ties; a PR whose base is the repo's default branch (or whose parent is not
loaded) is a root; a chain is a root plus its descendants in base order; roots with no children are not stacks. Fork PRs never chain,
as parent or child (the base ref is always in the base repo, and `POST /stacks` needs every head there too). Merged lower PRs: a loaded MERGED parent stays in the chain as a dim `merged` entry until
GitHub retargets the child (then the child becomes a root). Cycles are cut at the first repeat. Members outside the loaded list: for
the *open* PR only, `repository { parent: pullRequests(headRefName:$base, states:[OPEN], first:1) children: pullRequests(baseRefName:$head,
states:[OPEN], first:5) }` (cost 1, 0.7 s), walked to depth 10 and cached per (repo, branch) for 60 s. Native wins when both apply; an
unlinked PR based on a native stack's top is shown under it as `unlinked`.

**Needs rebase.** `baseRefOid` is the live base tip (#3's equals #2's `headRefOid`), so it cannot tell staleness; `mergeStateStatus`
`BEHIND` only appears under a "require up to date" rule (#1–#4 are all `CLEAN`). The real test is one aliased query per stack:
`ref(qualifiedName:"refs/heads/<child>") { compare(headRef:"<parent>") { aheadBy behindBy status } }` — **cost 1, 0.8 s for four links**;
`aheadBy > 0` ⇒ the child lacks `aheadBy` parent commits ⇒ `needs rebase (N behind <parent>)`. The bottom compares against the trunk
(informational: `behind main by 2`). Runs when the open PR's stack is built and with each detail refresh (20 s); never for list rows.

## 2. Where stacks show up

**List** (`⧉ 2/4` = position/size; native in accent, implicit in gray; `+` after it when the stack has unread members). With
`Group by stack` on (persisted, default on) `visible()` keeps each stack contiguous, bottom first, placed at its newest member's rank,
and the `#` cell carries tree glyphs; `z` collapses a stack to its bottom row (`⧉ 4 PRs · 2 unread`). Comfortable's second row
already holds change words, so the stack words go after them: `· needs rebase`.
```
   Repository                 #          Title                                     Author   CI  Review     Cmts   Updated
 • ● stealth-factory/gh-pr-m… #1 ⧉1/4    feat: detail tabs, actions and images     wiiiimm  ✓   —          3      2d
   ● stealth-factory/gh-pr-m… ├#2 ⧉2/4   feat: universal search palette            wiiiimm  ✓   ✓ approved 0      1d
 • ● stealth-factory/gh-pr-m… ├#3 ⧉3/4   feat: unread markers and refresh digest   wiiiimm  ✗   —          1 +1   3h
   ● stealth-factory/gh-pr-m… └#4 ⧉4/4   feat: tab badges, new markers, notifica…  wiiiimm  ●   —          0      1h
   ● stealth-factory/opscope  #439       fix(core): tmux pixel charts              copilot  ✓   ✓ approved 20     1d
```
Compact: same glyphs and chip (`├#2 ⧉2/4`), no extra rows. Collapsed: ` • ● stealth-factory/gh-pr-m… #1 ⧉ 4 PRs  feat: detail tabs…   · 2 unread`.

**Sidebar**: chip after the number; Cozy/Comfortable's dim second line ends with `· 2/4 of stack 196` or `· needs rebase` (accent) after
the change labels, repo and author shortened first; Compact shows `#3 ⧉3/4` only.
```
▌• ● ✗ #3 ⧉3/4 feat: unread markers and refresh digest
       gh-pr-manager · wiiiimm · 3h · +1 comment · needs rebase
```

**Detail — stack strip** in `info_lines`, right after the `base ← head` line and kept when the header is collapsed (the truncation
rule becomes "through the arrow line, plus the strip"): `Stack      main ← #1 ← #2 ← [#3] ← #4   3/4 · stack 196` (implicit: `· not linked`).
Each `#n` is an internal link `#pr:<id>` (opens it; spliced into the list like a palette hit when absent); `<` / `>` do the same.
Entries that are merged render dim with `✓`; `needs rebase` adds `!` after the number (`#4!`), conflicts `✗`.

**Detail — Stack tab** (`Tab::Stack`, 8th, key `8`, label ` 8 Stack 3/4 ` or ` 8 Stack – ` when unstacked). Two columns like Actions
(32 % / 24–56 cols, `│`). Left: `Whole stack` then the members top-down (GitHub's order), with state, CI, review, merge status and the
rebase signal. Right: the selected member — its status lines, then its files (`pulls/{n}/files`, cached per head oid, lazily), Enter on a
file shows the patch via `file_diff_lines`. `Whole stack` lists `compare/{trunk}...{top}` files (same `FileDiff` shape; 98 KB for one
link here; GitHub caps at 300 files, no patch beyond that). Unstacked PRs get one line: `Not in a stack · L links #1→#2→#3→#4 into a
GitHub stack` when an implicit chain exists, else `Not in a stack.`
```
 Stack 196 · 4 PRs · base main          │ #3 feat: unread markers and refresh digest          [O browser]
   Whole stack      main…#4  +812 −140 │ ● open · ✗ CI failed · review —  · ✓ ready to merge
   #4 ○ ● —   tab badges, new markers  │ parent #2 feat/universal-search · behind by 0 · 8 files +301 −12
▌  #3 ○ ✗ —   unread markers and refr… │ ── Files ─────────────────────────────────────────────────────
   #2 ○ ✓ ✓A  universal search palette │ ▌M src/app.rs                                       +201 −9
   #1 ○ ✓ —   detail tabs, actions …  !│   M src/ui.rs                                        +88 −3
   main                                 │   A src/changes.rs                                   +12 −0
                                        │ (Enter shows the diff · n p move · L link · M merge up to here · b update)
```
Member rows: `#n`, state (`○` open `◌` draft `◆` merged `✕` closed), `ci_icon`, review (`✓A` approved `✗C` changes `—`), title;
`!` at the right edge = needs rebase, `✗` = conflicts, `⊘` = blocked. Comfortable adds a blank row per member and a words line
(`needs rebase: 2 commits behind #2`); Compact drops both; Cozy as drawn. Colour never alone: every state has a glyph or a word.

## 3. Preview the whole stack

Two modes in the Stack tab: **per PR** (the member's own `pulls/{n}/files` — what GitHub shows, "only its own layer") and **cumulative**
(`Whole stack`, `GET /repos/{o}/{r}/compare/{trunk}...{top head}` → `files[]`, identical schema to `pulls/files`; for implicit chains
with a merged parent the trunk is the bottom open PR's base). Both reuse `draw_changes`' two-column layout and `file_diff_lines`,
cached in `tab_cache` under `{stack}:{member}:{oid}` so re-selecting is free. `n`/`p` move between members when the left column has
the cursor and between files once a member is opened (Enter), mirroring Changes; `Esc` goes back a level, then leaves the detail view.

## 4. Actions

| Action | How | Checkout? | Confirm text | v1 |
|---|---|---|---|---|
| Update one PR onto its parent | `updatePullRequestBranch(id, expectedHeadOid, REBASE)`; falls back to `MERGE` on error (`canBeRebased` is false on all of #1–#4, so REBASE may be refused) | no | `Rebase #3 (feat/change-signals) onto #2 (feat/universal-search)?` | yes |
| Link an implicit chain into a native stack | `POST /stacks {pull_requests:[bottom..top]}` after `stacks::link_plan` checks each base == previous head; 422 → toast with GitHub's message (also covers "not enabled") | no | `Link #1 → #2 → #3 → #4 into a GitHub stack on stealth-factory/gh-pr-manager?` | yes |
| Grow / dissolve a stack | `POST /stacks/{n}/add`, `POST /stacks/{n}/unstack` | no | | later |
| Merge up to here | `PUT /pulls/{n}/merge-async {sha: headRefOid, merge_method, merge_action: default}` → poll `/merge-async/{uuid}` every 2 s, 3 min cap; `enqueued` ends with "added to the merge queue" | no | `Merge #1, #2, #3 (up to #3) into main · squash · all or nothing` | yes, native only |
| Merge an implicit chain | refuse: `Link the chain first (L)`, since merging #1 leaves #2 targeting a deleted branch unless the repo deletes heads on merge | — | | — |
| Rebase the whole stack | `gh stack rebase` in a clone (cascading, conflicts possible) or, later, our own bottom-up loop of `updatePullRequestBranch` waiting for each `headRefOid` to move | yes | `Run gh stack rebase in ~/src/gh-pr-manager?` | later |
| Sync / checkout / submit | `gh stack sync`, `gh stack checkout <pr>`, `gh stack submit --auto` | yes | | later |
| Open the stack on GitHub | the bottom PR's page (its merge box holds the stack map; no stack URL is documented) | no | — | yes |

**Existing Merge menu.** For a PR with `stack != null` the menu's merge entries route to merge-async (plain merge is refused for
stacked PRs per the docs — confirm the GraphQL `mergePullRequest` behaviour on a throwaway stack before shipping) and the confirm text
lists the downstack PRs that merge with it. Every mutation goes through `Modal::Confirm` (`PendingOp::{UpdateBranch, LinkStack,
MergeStack, GhStack}`) and `Msg::Done`, so `own_pending`, the detail refetch and the list reload apply unchanged.

**Shelling out to `gh stack`** (later). Detection at start: `gh extension list`, parse the `gh stack … vX.Y.Z` line (cache; palette
entries disabled with `gh stack not installed · gh extension install github/gh-stack`; a notice when < v0.2.0). Clone resolution:
`[stacks] clones = { "owner/repo" = "~/src/repo" }` in config, else the cwd when `git remote get-url origin` matches the PR's repo;
otherwise the command is disabled with `no local clone configured`. Run with `tokio::process::Command` (`current_dir`, stdin null,
`GH_STACK_NO_UPDATE_NOTIFIER=1 GH_PROMPT_DISABLED=1 NO_COLOR=1 GH_NO_UPDATE_NOTIFIER=1`, explicit flags only — gh-stack skips its
prompts in a non-tty, so `merge` is never run without `--yes --<method>` chosen in our confirm). stdout and stderr are read line by line
into `Modal::Process { title, lines, scroll, exit: Option<i32> }` (a scrolling pane with `Esc` after exit, `Ctrl+C` kills the child);
exit codes map to toasts: 2 "not a stack", 3 "rebase conflict — finish with `gh stack rebase --continue` in <path>", 8 "stack locked",
9 "stacked PRs are not enabled for this repository". The child is killed on quit.

## 5. Keys and mouse

| Key | Where | Does | Conflict check |
|---|---|---|---|
| `8` | detail | Stack tab (`1-7` existing; `Tab::ALL` grows to 8) | free |
| `<` `>` | list, detail | previous (toward trunk) / next PR in the stack; stops at the ends with a toast | free (`.` is Menu; `<`/`>` are distinct chars) |
| `z` | list screen | collapse / expand the selected stack | `z` is Actions-tab only (`if actions`); list screen is free |
| `L` | detail, Stack tab, menu | link the implicit chain into a GitHub stack | free |
| `M` | Stack tab, menu | merge the stack up to the selected member… (method picker reuses the Merge menu) | free (`m` is the single-PR merge menu) |
| `b` | Stack tab, menu | update this PR's branch onto its parent (rebase, else merge) | free |
| `n p ↑ ↓ Enter O Esc` | Stack tab | move / open member / open in browser / back — same as Actions | per-tab bindings as today |
| click | chips, strip `#n`, member rows, `Whole stack`, files | select / open / show | `hits.stack_list: Option<(Rect, usize)>` like `file_list` |

Palette commands (`Command::…`, labels): `Stack: view` (opens the tab), `Stack: next PR` / `Stack: previous PR`, `Stack: update branch
onto parent`, `Stack: link into a GitHub stack`, `Stack: merge up to here…`, `Stack: rebase (gh stack)` (later), `Stack: open on GitHub`,
`Group by stack (on / off)`. `command_block` reuses the menu's disabled reasons (`not stacked`, `already linked`, `no write access`,
`gh stack not installed`). The Actions menu (Space) gains `L` / `M` / `b` entries with the same reasons. Help and footers gain the rows.

## 6. Interplay

- **Change signals.** `Snapshot` gains `stack: Option<(u64 number, usize size, usize position)>`; `Change::Stack { from, to }` (tier B:
  "added to stack 196", "stack grew to 5", "unstacked"). A merged lower member is already `State` on that PR; the upper PR gets
  `Change::Retargeted` from `baseRefName` (tier B, "now targets main"). Needs-rebase is detail-level only (compare runs for the open PR),
  so it feeds the Stack tab badge (`8 Stack 3/4 !`) and the in-visit digest, not list unread. Collapsed rows count unread members.
- **Search.** `pr_fields!` is shared, so local and GitHub hits show the chip for free; a `stack:196` qualifier filters locally (no
  GitHub equivalent; `search::parse` ignores it in remote queries) — later.
- **Repo filter / scopes.** Chains are per repo; grouping needs nothing else. `Everyone` + repo is the view that shows a whole stack
  when its members have different authors.
- **Refresh.** List (60 s) refreshes chips and chains; detail (20 s) refreshes entries and runs the compare query (1 point); missing-link
  lookups are cached 60 s; stack files follow the Changes cadence (per head oid). A merge-async poll is one REST call per 2 s while pending.

## 7. Rust-level additions

```rust
// src/stacks.rs (new, pure, unit-tested; no App access)
pub enum Origin { Native { id: String, number: u64 }, Implicit }
pub struct Member { pub id: String, pub number: u64, pub position: usize, pub state: String, pub is_draft: bool,
    pub head: String, pub base: String, pub head_oid: String, pub title: String, pub loaded: bool }
pub struct Stack { pub repo: String, pub origin: Origin, pub trunk: String, pub members: Vec<Member> /* bottom first */ }
pub struct Link { pub child: String, pub parent: String, pub behind: usize, pub status: String }   // one compare result
pub fn chains(prs: &[PrSummary], default_branch: &dyn Fn(&str) -> Option<&str>) -> Vec<Stack>;   // implicit, same repo
pub fn from_detail(d: &PrDetail) -> Option<Stack>;                                                  // native, entries
pub fn membership(stacks: &[Stack]) -> HashMap<String, (usize, usize)>;                            // pr id → (stack, position)
pub fn needs_rebase(links: &[Link], child: &str) -> Option<usize>;                                 // commits behind the parent
pub fn grouped(order: &[usize], prs: &[PrSummary], m: &HashMap<String,(usize,usize)>, collapsed: &HashSet<String>) -> Vec<RowView>;
pub struct RowView { pub idx: usize, pub glyph: Option<&'static str> /* "├" "└" */, pub hidden: usize, pub hidden_unread: usize }
pub fn breadcrumb(s: &Stack, current: &str) -> Vec<Crumb>;                                        // trunk ← #1 ← [#3] ← #4
pub fn neighbour(s: &Stack, current: &str, up: bool) -> Option<&Member>;
pub fn link_plan(s: &Stack) -> Result<Vec<u64>, String>;                                            // base == previous head, open only
pub fn merge_plan(s: &Stack, up_to: &str) -> Result<Vec<&Member>, String>;                        // open downstack, no drafts
pub fn exit_code_message(code: i32) -> &'static str;                                               // gh-stack 2/3/4/8/9

// github.rs
pub struct StackRef { pub id: String, pub number: u64, pub size: usize, pub base_ref_name: String }
pub struct StackEntryRef { pub position: usize }
pub struct StackDetail { #[serde(flatten)] pub r: StackRef, pub entries: Counted<StackEntry> }
pub struct StackEntry { pub position: usize, pub pull_request: Option<PrSummaryLite> }   // the signal fields above
// PrSummary: base_ref_name, stack: Option<StackRef>, stack_entry: Option<StackEntryRef> (all #[serde(default)])
// PrDetail: base_ref_oid, stack: Option<StackDetail>, repository.default_branch_ref
impl Client {
    pub async fn compare_links(&self, nwo: &str, pairs: &[(String, String)]) -> Result<(Vec<Link>, Option<RateLimit>)>; // aliased GraphQL
    pub async fn chain_links(&self, nwo: &str, head: &str, base: &str) -> Result<(Option<PrSummary>, Vec<PrSummary>, Option<RateLimit>)>;
    pub async fn compare_files(&self, nwo: &str, base: &str, head: &str) -> Result<Vec<FileDiff>>;   // REST compare, files[]
    pub async fn update_branch(&self, pr_id: &str, head_oid: &str, method: UpdateMethod) -> Result<()>;
    pub async fn create_stack(&self, nwo: &str, numbers: &[u64]) -> Result<u64>;                     // POST /stacks → number
    pub async fn merge_async(&self, nwo: &str, n: u64, sha: &str, m: MergeMethod) -> Result<MergeJob>; // PUT merge-async
    pub async fn merge_async_status(&self, nwo: &str, n: u64, uuid: &str) -> Result<MergeJob>;
    async fn rest_put_json / rest_post_json(&self, url, body) -> Result<(StatusCode, Value)>;        // new verbs, same headers
}
pub struct MergeJob { pub status: String, pub uuid: Option<String>, pub error: Option<String> }

// app.rs
Tab::Stack (ALL = 8, keys '1'..='8');  Msg::{Links{id, res}, Chain{id, res}, StackFiles{key, res}, MergeJob{id, res}, Process(ProcessMsg)}
Action::{StackPrev, StackNext, StackCollapse, StackSelect(usize), StackOpenMember, StackLink, StackMergeUpTo, StackUpdateBranch,
    StackOpenBrowser, ToggleGroupStacks, GhStack(GhStackCmd)}
PendingOp::{UpdateBranch { oid: String }, LinkStack(Vec<u64>), MergeStack { up_to: u64, method: MergeMethod, sha: String }, GhStack { cmd, cwd }}
Modal::Process { title: String, lines: Vec<String>, scroll: usize, exit: Option<i32> }
pub stacks: Vec<stacks::Stack>, pub membership: HashMap<String,(usize,usize)>,   // rebuilt in finish_list and on Msg::Detail
pub links: HashMap<String, (Vec<Link>, Instant)>, pub collapsed: HashSet<String>, pub stack_sel: usize, pub stack_files: HashMap<String,(Vec<FileDiff>,u64)>
// state.rs: pub group_stacks: bool (default true).  config.rs: pub stacks: StacksConfig { clones: HashMap<String, PathBuf> }
// visible(): grouped() runs after the unread filter and before unread-first; selection is kept by id (pending_select) across regroups.
```
`tokio::process` is already a feature of the `tokio` dependency; nothing new is linked.

**Tests.** `chains`: OPEN head preferred over an older CLOSED PR on the same branch; a MERGED parent stays as a merged entry; a child
retargeted to `main` becomes a root; forks never chain; same branch names in two repos never chain; a cycle (`a→b→a`) ends at the repeat;
single PRs are not stacks. `from_detail`: positions sort bottom-first, members outside the list are `loaded == false`. `grouped`: stacks
contiguous and bottom-first at the newest member's rank, glyphs `├`/`└`, collapse hides members and counts their unread, the pinned
search and the unread filter compose, selection survives a regroup. `breadcrumb`/`neighbour`: ends stop. `link_plan`: rejects a gap,
a draft is allowed, a merged bottom is skipped. `merge_plan`: open downstack only, a draft or closed member blocks with its reason.
`needs_rebase` from `aheadBy`. Merge-async state machine: pending → merged, pending → failed(error text), enqueued, 3-min timeout,
and `Msg::Done` semantics (own_pending, refetch). `exit_code_message` table. UI: the chip renders in all three densities, Compact
has no second row, `8` selects the tab, `<`/`>` select the neighbour and splice an unloaded member.

## 8. MVP versus later

**MVP (one PR, no behaviour change for unstacked PRs):** list and detail query fields (cost unchanged); `stacks.rs` with `chains`,
`from_detail`, `membership`, `grouped` (no collapse), `breadcrumb`; chips in table, sidebar and palette rows; group-by-stack toggle
(palette) with tree glyphs; the strip in the info header with clickable `#n` and `<`/`>`; the Stack tab with members, per-member files
and `Whole stack` (compare); needs-rebase via `Ref.compare`; actions `b` (update branch), `L` (link via REST), `M` (merge-async, native
stacks, with the downstack list in the confirm) and "open on GitHub"; the Merge menu routed to merge-async for native members.

**Later:** `z` collapse; missing-link lookups beyond the loaded list; `gh stack` process modal (rebase, sync, checkout, submit) with
clone configuration and extension detection; our own bottom-up server-side cascade; add/unstack; `Change::Stack`/`Retargeted` and the
Stack tab badge; `stack:N` search qualifier; merge-queue states (`enqueued` → "in queue, position …" needs `mergeQueueEntry`);
`bypass_rules` for admins.

**Surprises and risks.** (1) gh-stack is three releases behind locally and still "public preview" upstream with 150 open issues; pin
the CLI path to "later" and build v1 on the documented REST/GraphQL surface, which covers link, merge and per-PR update without a
checkout. (2) `PUT /merge` refuses stacked PRs: the existing merge path must branch on `stack`, and the GraphQL `mergePullRequest`
behaviour on a native member is unverified (test on a throwaway repo before release). (3) Whether a repo has stacks enabled is only
learnt on `POST /stacks` (422 / exit 9); `GET /stacks` returned `200 []` for this repo, so a successful list is not proof. (4) `baseRefOid`
is live, not historical, so staleness needs the compare query (1 point per stack per detail refresh). (5) `canBeRebased` is false on all
four chained PRs; `updatePullRequestBranch(REBASE)` may be refused and must fall back to MERGE with a toast. (6) The compare endpoint
returns full patches (98 KB for one link, 300-file cap) — `Whole stack` must stay lazy. (7) `first:100` already breaks the org list
query; the stack fields add no cost at 50 but the limit must not move. (8) No open native stack exists in the account's orgs today, so
the Stack tab's live validation will have to happen on a throwaway stack (`gh stack link` on two scratch PRs) during implementation.

## Implementation notes

v1 as built (branch `feat/stacks`), and where it departs from the design above.

**Detection.**
- The list query gains `baseRefName isCrossRepository stack { id number size baseRefName } stackEntry { position }`. `isCrossRepository` is new: it is how "forks never chain" is enforced. Measured on `org:stealth-factory is:open`, `first:50`: cost **1 → 1**. The page size stays 50.
- The detail query gains `stackEntry`, `stack { … entries(first:100) { … } }` and `repository.defaultBranchRef`. Measured at `entries(first:30)`: cost **2 → 2**; raised to 100 so a large stack is never truncated (unmeasured, at most about a point more: one `commits(last:1)` per extra entry). `baseRefOid` is not fetched, because nothing reads it: the compare query is the staleness signal.
- Needs-rebase uses one aliased `ref(child).compare(headRef: base)` query per stack. Measured on the #1→#4 chain: **cost 1**. It pairs every *open* member's head with its own base, so the bottom one is compared with the trunk. That bottom member shows `needs rebase` when the trunk moved, not just an informational `behind main`.
  - The query runs with each detail refresh, at most once every 10 s, and when the open PR changes (stale after `detail_refresh_secs`, at least 20 s).
  - A failed query keeps the last answer. It is skipped while GraphQL `remaining < 100`.
- `stacks::build` runs in this order:
  1. Native stacks are built from the list's fields.
  2. Each one is replaced by a loaded detail's full entry list when one exists. Members outside the list are marked `loaded = false`.
  3. Implicit chains are built among the PRs that no native stack claims.
  4. A detail whose stack has no loaded member, such as one left over from another repo filter, is ignored.
- **Branching chains:** when two PRs target the same branch, the preferred child continues the chain (OPEN over MERGED over CLOSED, newest on ties). The other child starts a chain of its own. The doc did not say how to handle this.
- **Unknown default branch:** until a detail of that repo has loaded, `main` and `master` count as trunks. Without this, a `main → release` PR would become the parent of every PR based on `main`.

**List.**
- Grouping runs *after* unread-first. The design put it before, but sorting afterwards would split the stacks apart.
- The `#` column widens to fit `├#2 ⧉2/4`. The sidebar shows the glyph and chip too.
- The Cozy and Comfortable sidebar line ends with `· 2/4 of stack 196`, `· 2/4 · not linked` or `· needs rebase`. The Comfortable table adds `needs rebase (N behind)` to its words row.
- The chip itself tells native from implicit by colour only. The words in the sidebar, the strip and the Stack tab carry that difference too.
- Palette rows show the chip, including GitHub hits that are not loaded (taken from their own `stack` fields).

**Strip.**
- The `base ← head` info row is now tagged with a zero-width link. This lets the collapsed header and the strip find that row even when it wraps. It also fixes a latent bug: a long branch pair used to break the collapse.
- A strip too wide for the pane folds the members farthest from the open PR into `…`.
- Merged members show `✓` and closed ones `✕`. `!` marks needs-rebase and `✗` marks conflicts. `⊘` appears only in the Stack tab.

**Opening a member.** `<`, `>` and the strip's `#n` select the member when it is loaded. Otherwise they fetch it with `prs_by_number` and splice it into the list like a palette hit. A loaded member hidden by a pinned search clears the pin, the same way the digest's jump does.

**Stack tab.**
- The cursor starts on the open PR, not on `Whole stack`, so the cumulative compare (up to 300 files with patches) only loads when you ask for it.
- Enter, or a click on a file, opens a file's patch, with a `‹ back` button and `Esc`. `n`/`p` then move between files. Members' files reuse the Changes cache (`files`, keyed by PR id and head oid).
- `Whole stack` compares the lowest *open* member's base with the top member's head oid.
- In Compact density the tab looks the same as in Cozy. Comfortable adds a blank row and a words row per member.

**Tab bar.** Eight tabs don't fit the detail pane on a 160-column terminal. When they don't fit, inactive tabs switch to short names (`Desc Conv Diff Log Checks Runs Revs Stack`), then to numbers only. The active tab keeps its full name.

**Actions.**
- `b`, `L` and `M` work in the list, the detail view and the Stack tab. In the Stack tab they act on the cursor's member; elsewhere, on the selected PR. `b` requires the PR to be in a stack.
- In the Space menu, update-branch is **`B`**, because `b` already opens the browser there.
- `M` opens a small method picker with the methods the repo allows. The design called for reusing the full Merge menu.
- A native member picked from the existing Merge menu goes to the same merge-async confirm, which lists the downstack PRs.
- Merging an implicit chain with `M` is refused: `not a GitHub stack: link it first (L)`. A plain `m` merge of an implicit member is still allowed. Its confirm warns that the PR above won't be retargeted.
- The merge-async poll runs inside the spawned task and ends in `Msg::Done`, so there is no `Msg::MergeJob`. It polls every 2 s for up to 3 min, and a 409 reply follows the pending request's uuid. The header spinner shows while it is busy; there is no separate progress toast.
- The stacks and merge-async REST calls send `X-GitHub-Api-Version: 2026-03-10`.
- A 422 from `POST /stacks` whose text says the feature is not enabled becomes "Stacked PRs are not enabled for this repository — HTTP 422: …". Other 422s show GitHub's message and errors.

**Untested live.** No mutation was sent while building this. On a throwaway stack, still to try:
- `updatePullRequestBranch` REBASE, and its MERGE fallback;
- `POST /stacks`, including the "not enabled" 422;
- `PUT …/merge-async` with polling;
- the Merge menu's merge-async routing on an open native member.

**Not built (later, as planned).**
- `z` collapse;
- missing-link lookups (`chain_links`);
- an `unlinked` PR on a native stack's top;
- the gh-stack shell-outs and `Modal::Process`;
- add / unstack;
- `Change::Stack` / `Retargeted` and the Stack tab's `!` badge;
- the `stack:N` qualifier;
- merge-queue positions;
- `bypass_rules`.
