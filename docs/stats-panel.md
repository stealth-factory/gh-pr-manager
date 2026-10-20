# Stats panel (opened / merged per day, last 24 h) — design

Request: "look into opscope's implementation of github pr widget and see how it implemented the open and merge / day charts and
last 24 hrs figures. can we add those in our tool and make it a toggleable display that is sticky to the bottom of the screen
(above our footer info)". opscope was read from `origin/main` at `175a6bc` (2026-10-17, "feat(core): add Kitty notifications and
large text for clocks and months"); a `git fetch` on 2026-10-20 confirmed that is still the tip. All GraphQL below was validated
read-only with `gh api graphql` on 2026-10-20 (account `wiiiimm`, 9 orgs); timings are wall-clock and **include ~0.5 s of `gh`
overhead**; cost is `rateLimit.cost`.

## 1. How opscope does it (`widgets/src/widgets/github-prs/main.rs`)

**Figures.** Four sections above its PR list, toggled by `t`: STATE (a stacked bar of review decisions, drafts, conflicts, ready to
merge — read off the open pool), **OPENED / DAY** and **MERGED / DAY** (30 calendar days, GitHub's own counts), and AGE OF OPEN PRs
(one bar per open PR). Each day chart carries one figure drawn large to its right: `opened · last 24h` / `merged · last 24h`. That is
the whole "last 24 h" block — two numbers; no closed, reviews or comments (the sibling `github` widget adds `o0_dropped`, closed
unmerged over the window, and R24/T2D medians, but those are per-account board metrics, not the day charts). Its README explains the
choice: the pool is `is:open`, so "bucketing the open pool by `createdAt` counted only the arrivals still sitting in it, 26 of 677".

**Queries.** Aliased `search(type:ISSUE) { issueCount }`, one alias per UTC calendar day, two requests in flight at once:

```
m{n}: search(query:"{scope} is:pr is:merged merged:{YYYY-MM-DD}", type:ISSUE) { issueCount }   # ×30, + the two windows:
mw:   search(query:"{scope} is:pr is:merged merged:>={now-24h as %Y-%m-%dT%H:%M:%SZ}", type:ISSUE) { issueCount }
cw:   search(query:"{scope} is:pr created:>={same}", type:ISSUE) { issueCount }   rateLimit { remaining limit }
c{n}: search(query:"{scope} is:pr created:{YYYY-MM-DD}", type:ISSUE) { issueCount }            # ×30, second request
```
`scope` is `@mine` = `org:a org:b … user:me` (repeated owner qualifiers are OR'd). No `is:open`, so a PR opened and merged the same
afternoon counts on both charts. It is split in two because "thirty-two merge aliases answer in 4.0–4.3 s, sixty-two … in one request
take 8.0–8.1 s — at the gateway's ~10 s cliff"; "an `issueCount` costs one rate-limit point per *request* however many aliases ask
for it". Both ride the list's 60 s refresh, after the owner walk and before the list paging, and report through their own
`counts_err` / `created_err` so a refused count never costs the list. No caching across runs; `rateLimit` is read off every answer.

**Bucketing.** `chart_days()`: `Utc::now().date_naive()`, 30 days ending today (today is partial — the labels say `today`, and the
24 h figure is the rolling complement). UTC "because GitHub reads a bare `merged:YYYY-MM-DD` … in UTC". The chart looks a day up
**by its date string, not by position**, "so a set of counts fetched before midnight cannot slide every figure one day along".
`read_counts` is "deliberately not `unwrap_or(0)`": a missing alias fails the whole series and the chart shimmers with `counting`.

**Rendering** (text, no braille): `tc::vbars(cols, 3, peak)` — three rows of eighth blocks, linear against the peak:
```rust
const EIGHTHS: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
let top = hi * (height - r) as f64 / height as f64;  let bottom = hi * (height - r - 1) as f64 / height as f64;
let ch = if *value >= top { '█' } else if *value <= bottom { ' ' } else { EIGHTHS[(((value-bottom)/(top-bottom))*8.0) as usize).clamp(1,8)] };
```
Bar width: `slot = avail / 30` cells per day, a 1-cell gap when `slot >= 3`; a `─` baseline the exact span; axis labels `30d ago …
today` (the left label dropped when the span is too narrow). Caption: ` ── OPENED / DAY ── last 30d · 243 opened · peak 37/day`.
Opened bars in the PR colour, merged in green, and "the figure is the same count as the bars beside it, so it is the same colour".
The large figure is a 3×5 pixel font on half blocks — "two pixel rows to a cell", three text rows tall:
```rust
const DIGITS: [[&str; 5]; 10] = [["###","# #","# #","# #","###"], ["  #","  #","  #","  #","  #"], ["###","  #","###","#  ","###"], …];
row.push(match (lit(glyph.get(r * 2)), lit(glyph.get(r * 2 + 1))) { (true,true) => '█', (true,false) => '▀', (false,true) => '▄', _ => ' ' });
// big_digits(0) == ["█▀█", "█ █", "▀▀▀"]
```
Widths (`split_halves`): the figure column is `max(label) + 2` when the row has room for `30 + 2` chart cells beside it, else
`FIGURE_DIGITS (11, three digits) + 2` with the label wrapped onto two lines, else zero and "the number sits on its own row under the
caption. A narrow pane loses the size of the number, never the number." Four or more digits drop to plain text. Unknown states are
worded, never zero: `loading` (shimmer), `counting`, `not counted`, `count failed: <why> · counted 12m ago`, `· 24h count stale`.
No height gate ("a section that is not drawn looks exactly like a section with nothing in it"); the whole widget scrolls instead.
The `github` widget's PR FLOW is the mirrored variant (opened up, merged down, `opened 24h` labels, figures stand down under a
20-column chart) — more height, not wanted here.

## 2. Figures and scope for gh-pr-manager

Show **opened / day** and **merged / day** for the last `days` (default **14**, max 30) and a **LAST 24 H** block of **opened,
merged, closed** (closed = `is:closed is:unmerged closed:>=`, the one extra GitHub can count; reviews and comments are not
searchable aggregates and would need node paging — not offered). Skip STATE and AGE: this app's list already shows CI/review state
per row, and a bottom panel has no room for a per-PR age chart.

**Scope: follow the scope chip and the repo chip; ignore the state filter, the unread filter and the pinned search.** In opscope the
board is `@mine`; here the scope *is* the board, and the figures must describe what the list describes (`org: stealth-factory`,
`My PRs`, `All PRs (my repos + orgs)`, a repo). The state filter is ignored by construction — merged/day needs merged PRs whatever the
chip says — and the caption names the scope so this is never ambiguous. Validated: `org:`, the 10-owner "All PRs" string,
`author:@me`, `review-requested:@me` and `repo:` all accept the day qualifiers at cost 1. Caveat: `review-requested:@me` counts 0
merges (a merged PR no longer carries a pending request) — the merged caption says `not countable for this scope`.
Switching: `[stats] scope = "follow"` (default) or any qualifier string (`"author:@me"`, the opscope `@mine` feel is
`"user:me org:…"`); palette command `Stats: follow the scope / count my PRs` flips between `follow` and `author:@me` for the session.

## 3. Queries, cost, cadence, cache

Two requests per refresh, concurrent, exactly opscope's split (merged days + the three 24 h windows; created days), scope =
`search_query()` minus `is:pr archived:false`, the state qualifier and `sort:`. Rolling cut `since = now − 24 h` as a full UTC
datetime (a bare date means "today so far", which reads as a collapse every morning):

```
{ m0: search(query:"org:stealth-factory is:pr is:merged merged:2026-10-07", type:ISSUE){ issueCount } … m13: …
  mw: search(query:"org:stealth-factory is:pr is:merged merged:>=2026-10-19T04:12:00Z", type:ISSUE){ issueCount }
  cw: search(query:"org:stealth-factory is:pr created:>=2026-10-19T04:12:00Z", type:ISSUE){ issueCount }
  xw: search(query:"org:stealth-factory is:pr is:closed is:unmerged closed:>=2026-10-19T04:12:00Z", type:ISSUE){ issueCount }
  rateLimit { cost remaining limit } }
{ c0: search(query:"org:stealth-factory is:pr created:2026-10-07", type:ISSUE){ issueCount } … c13: …  rateLimit { … } }
```

| Scope | Request | Aliases | Latency (via `gh`) | Cost |
|---|---|---|---|---|
| `org:stealth-factory` | 14 d merged + created + 24 h, one request | 31 | 4.1–4.7 s | 1 |
| `org:stealth-factory` | 24 h block only | 3 | 0.8–1.0 s | 1 |
| `org:stealth-factory` | 30 d both + 24 h, one request | 63 | **9.6–10.6 s** (the ~10 s cliff) | 1 |
| All PRs (user + 9 orgs) | 14 d both + 24 h, one request | 31 | 4.4–4.5 s | 1 |
| All PRs | 30 d both + 24 h, one request | 63 | 6.2–8.2 s | 1 |
| All PRs | 30 d merged only / created only (split) | 30 / 30 | 3.1–4.0 s / 3.8–4.3 s | 1 each |
| `author:@me` · `review-requested:@me` · `repo:…` | 7 d both + 24 h | 17 | 3.7 s · 2.9 s · 1.7 s | 1 |
| rejected: `merged:>=30d` nodes `{ mergedAt }`, first:100 | one org = 294 PRs → 3 pages | — | 1.3 s/page | 1/page |

Counts came back identical across runs. Node paging was rejected for the reason opscope gives: `issueCount` is exact at any volume
for one point, and the all-orgs scope would page far beyond three. Cost is 2 points per refresh.

**Cadence:** `[stats] refresh_secs = 300` (the figures are a 14-day shape and a 24 h window; 5 min is plenty, 24 points/hour against
the list's 60+). Also refetch on `r`, on a scope/repo change (the cached series for the old key is dropped from view, the new key's
cache paints first), on the UTC date rolling over (the axis moves; the old series still draws, looked up by date, with today's bar
dim `·` and `counting` until the answer lands), and when the panel is shown after being hidden with figures older than the cadence.
**Never fetch while hidden**, auto-hidden, or while a modal that pauses the list (composer, palette) is open.

**Guards:** skip a refresh (caption `paused · API budget low`) when `rate.remaining < limit / 10` — the header already turns red
there; a failed request backs off 5 → 10 → 20 min and keeps the last good figures with `count failed: <why> · counted 12m ago`;
a missing alias fails that series (never a silent zero); a GraphQL `errors[]` on one request leaves the other chart alone.

**Cache (`state.json`):** `stats_cache: HashMap<String, StatsCache>` keyed by the scope string, value `{ days: [(date, opened,
merged)], window: {opened, merged, closed}, at: unix secs }`, capped at 8 keys, entries older than 7 days dropped on save. First
paint shows the cached series with `counted 3h ago` dim in the caption and refetches at once — instant, honest, and never a fake
zero. Older state files load unchanged (`#[serde(default)]`).

## 4. Layout

`ui::draw` becomes `[header, body, stats?, status, footer]`: the panel is a full-width `Constraint::Length(h)` directly above the
status line, on the list screen and the split detail view alike (the sidebar/detail split sits above it). It is drawn before
`dim()`, so modals fade it with everything else, and it never uses the status line, so the change-signals digest and toasts are
untouched. Height by density: **Comfortable 9, Cozy 7, Compact 1.** Cozy at 120 columns (bars 2 cells, no gap; `▲` under today):

```
─┤ ▂▅ Stats ├─ org:stealth-factory · last 14d · UTC days · counted 2m ago ──────────────────────────────── T hide ──
  OPENED / DAY  206 opened · peak 44        MERGED / DAY  184 merged · peak 43        LAST 24H  (rolling)
          ██  ██                                    ██  ██                              █ █ █    █▀█    █▀█
      ▆▆  ██  ██                                ▃▃  ██  ██                              █ ▀▀█    █▀█    █ █
  ▃▃▂▂██▂▂██▂▂██▄▄▁▁▁▁▁▁▂▂▂▂▂▂                  ▃▃▃▃██▂▂██▄▄▂▂▁▁▂▂▁▁▂▂▁▁                ▀   ▀    ▀▀▀    ▀▀▀
  ────────────────────────────▲                 ────────────────────────────▲           opened   merged  closed
  14d ago                  today                14d ago                  today
```
Comfortable adds a blank row under the rule and under the labels; Compact is one row, no rule, sparklines of `SPARK`-style eighths:
```
 Stats org:stealth-factory 14d  OPENED 206 ▃▂█▂█▂█▄▁▁▁▂▂▂ pk 44  MERGED 184 ▃▃█▂█▄▂▁▂▁▂▁▁▁ pk 43  24h 14 opened · 8 merged · 0 closed
```
Colours: opened bars and the opened figure in `ACCENT` (cyan), merged in green, closed in gray; today's bar is bold **and** carries
the `▲` on the baseline; the heading words carry the meaning so nothing relies on colour alone. No emoji (`▂▅`, `▲`, `─` only).

Width budget, left to right: 2-cell margin, chart A, 4-cell gutter, chart B, 4, figure block. The figure block is three
`FIGURE_DIGITS = 11`-cell slots (fixed at three digits so the column never jumps) = 37 cells; `slot = chart_w / days`, gap when
`slot >= 3`. Fallbacks: **< 100 columns** the figure block stands down and the 24 h numbers move into the heading row as plain text
(`LAST 24H 14 opened · 8 merged · 0 closed`); **< 64** the Compact single row is used at any density; **< 44** the panel is not drawn
(the chip stays). **Too short:** when `rows < 3 + panel_h + 8` (eight list rows is the floor) the panel auto-hides without changing
the toggle, and reappears when the window grows. Four-digit figures drop to plain text rather than being cut, as in opscope.

## 5. Toggle

- **Key `T`** everywhere outside modals and text inputs. `t` was rejected: the Actions tab already uses it (log timestamps) and the
  panel is visible on that tab, so the same key would mean two things on one screen. `T` is free in every mode (checked against the
  global, Actions and Stack tables). Help gains `T / click ▂▅ Stats` under *Everywhere*.
- **Header chip** ` ▂▅ Stats ` after ` ⌕ Search ` (underlined when on, `Action::ToggleStats`); the panel's rule carries ` T hide `;
  the list footer gains `("T", "Stats", Action::ToggleStats)` after `u Unread`.
- **Palette:** `Command::Stats` = "Show / hide the stats panel", `Command::StatsScope` = "Stats: follow the scope / count my PRs".
- **Persisted:** `state.stats_visible: Option<bool>` (None → `cfg.stats.visible`, default `true`), saved on toggle.

## 6. Interaction

- **Click a day bar** (either chart) → `Action::StatsDay(Some(i))`: both charts highlight that day (REVERSED + `▲` moves to it) and
  the heading row reads `Wed 15 Oct (UTC) · 34 opened · 17 merged`; click it again, or the `✕` after that text, to clear. The
  figures are read off the series already in hand — no request.
- **Wheel** over the panel moves the day cursor (up = earlier, down = later; starts at today); nothing scrolls, so the list's wheel
  behaviour is unchanged. Clicking the ` T hide ` text or the `✕` clears/hides.
- **Filtering the list to that day (later):** double-click → set the state chip (Merged for the merged chart, All for opened) and pin
  `merged:2026-10-15` / `created:2026-10-15` in the universal search. The local query grammar has no date qualifiers and the list is
  capped at 300, so this needs `created:`/`merged:` in `search::Query` (local: compare the PR's dates; remote: pass through to
  GitHub, which already accepts them) — docs/search.md §5. Not in the MVP.
- **Modals:** dimmed like everything else; the palette and help are centred over the body and may overlap the panel — fine.
- **Change signals:** independent. The digest diffs the list; the panel is a different population (everything in scope, any state)
  and a different window, so a `+2 merged` digest and the merged/day bar are not expected to agree. Bots count (renovate, copilot)
  as they do in opscope; `[stats] exclude_bots` via `-author:` qualifiers is a later option.

## 7. Rust-level additions

- **`src/stats.rs`** (pure, no I/O, no new crate): `utc_today(now: SystemTime) -> (y, m, d)` and `date_string` via the civil-from-days
  algorithm (the project has no chrono); `chart_days(today, n) -> Vec<String>`; `scope_of(search_query) -> String`;
  `count_query(scope, days, since, Which::{Merged, Created}) -> String`; `read_series(&Value, days, prefix) -> Result<Vec<(String,
  i64)>>` and `read_window(&Value) -> Result<Window>` (refuse missing aliases); `series_for(days, have)`; `bars(values, height,
  peak) -> Vec<Vec<char>>`; `big_digits(i64) -> [String; 3]`; `Layout::fit(width, days, density) -> Layout { mode: Full | Spark |
  Hidden, chart_w, slot, gap, figure_w }`; `panel_height(density, mode)`; `day_at(layout, x) -> Option<usize>`; `caption(...)`.
- **`github.rs`:** `pub async fn count_search(&self, query: &str) -> Result<(Value, Option<RateLimit>)>` (the existing
  `graphql::<Value>` plus `rateLimit { cost }`).
- **`config.rs`:** `pub stats: StatsConfig { days: u8 (14, clamped 7..=30), visible: bool (true), scope: String ("follow"),
  refresh_secs: u64 (300) }` under `[stats]`.
- **`state.rs`:** `stats_visible: Option<bool>`, `stats_cache: HashMap<String, StatsCache>`, `prune_stats(now)`.
- **`app.rs`:** `Msg::Stats { key, which: Which, res: Result<(Value, Option<RateLimit>)> }`; `App.stats: Stats { key, merged:
  Option<Series>, created: Option<Series>, window: Option<Window>, at: Option<Instant>, err: [String; 2], inflight: u8, day:
  Option<usize>, backoff: u32 }`; `Action::ToggleStats`, `Action::StatsDay(Option<usize>)`, `Action::StatsScope`;
  `Hits.stats: Option<Rect>`, `Hits.stats_days: Vec<(Rect, usize)>`; `on_tick` drives the cadence; `layout_key` gains `stats shown`
  so the body repaints when the panel appears (the image-cell repaint trick); `Command::Stats` / `StatsScope` in `search.rs`.
- **`ui.rs`:** `draw_stats(f, app, area)`; `draw` splits off the panel row; `draw_help` and `draw_footer` rows above.
- **Tests:** `chart_days` across a month end and 2028-02-29; `count_query` has exactly `days (+3)` aliases and the ISO datetime cut;
  `read_series` fails on one missing alias and parses the 14-day fixture above; `bars`: 0 → space, peak → `█`, half → `▄`;
  `big_digits(0) == ["█▀█","█ █","▀▀▀"]`, every row the same width; `Layout::fit` at 40/60/80/100/120/200 columns never exceeds the
  width and stands the figures down below 100; `day_at` round-trips every bar; `StatsCache` round-trips and a pre-stats
  `state.json` loads; a `TestBackend` draw (as `comfortable_rows_carry_words_and_clicks_hit_the_right_pr` does) asserts the panel
  sits on the row above the status line, the table shrank by `panel_h`, `hits.stats_days` has `days` rects, and a 20-row terminal
  auto-hides it with `stats_visible` untouched; `scope_of` strips `is:open`, `archived:false` and `sort:`.

## 8. MVP versus later

**MVP:** stats.rs, the two requests with cache and guards, `T` / chip / palette toggle, the Cozy/Comfortable/Compact panel with
width and height fallbacks, click-a-day readout, state persistence, config `[stats]`, tests above.
**Later:** double-click filtering (needs date qualifiers in the search grammar), `exclude_bots`, a `days` picker in the palette,
a `review-requested` fallback scope for the merged chart, Kitty/Sixel pixel bars like opscope's `Plot::bars` (the text bars are the
baseline and must stay as the fallback), weekday ticks on the axis for 30-day windows.

## Differences from opscope, and why

14 days by default rather than 30 (a bottom panel at 100–120 columns gives 2-cell bars for 14 days, 1-cell for 30, and 30 in one
request brushed the 10 s cliff); charts side by side rather than stacked (height is the scarce axis in a pinned panel); a third
24 h figure, closed, because it is one more alias at no cost; the scope follows the chips instead of `@mine`; a cache in
`state.json` for an instant first paint and a 5-minute cadence instead of riding the 60 s list refresh; and a clickable day cursor,
since opscope's wall display has no pointer interaction. Everything about honesty is kept: UTC days looked up by date, a missing
alias is `counting` and never 0, the figure is the colour of its bars, and a narrow pane loses the size of a number, never the number.

## Implementation notes

What shipped (MVP, `feat/stats-panel`), and where it departs from the design above.

- **The 24 h block is the hero** (user request after the first preview: "highlight the 24 hrs stats instead of the 14d
  stats"). It comes first, on the left (`MARGIN`, 37 cells), in the big 3×5 half-block digits, bold and in each series'
  colour, under a bold `LAST 24H` heading. Under each figure: its word and the change against the 24 h before it (`opened
  ▲3`, `merged =`, `closed ▼2`; the heading says `▲▼ vs the 24h before`). The 14-day charts are secondary: they take the
  remaining width to the right, their headings carry no bold, captions and axis labels are dim.
- **Previous 24 h:** three more aliases in the merged request — `cp` / `mp` / `xp` with a datetime range
  `created:{now-48h}..{now-24h}` (verified: the range count equals the difference of the two `>=` counts). Still cost 1.
  A missing previous alias just drops the deltas (`read_prev_window` → `None`); the figures never depend on it.
  `StatsCache` gained `prev` (`#[serde(default)]`).
- **Fallbacks, hero-first:** the charts are dropped before the figures ever shrink. `Layout::fit`: < 24 columns hidden;
  < 40 (or Compact at any width) the one-row form; otherwise the big figures always, plus both charts when each still gets a
  cell a day (14 days: from ~80 columns). The design's "figures stand down below 100 columns" no longer applies.
- **Compact / one row:** `LAST 24H 14 opened ▲3  8 merged =  0 closed ▼2 │ 14d opened ▃▂█…  merged ▃▃█…` — the 24 h
  numbers first in full colour; sparklines only if they fit whole (deltas go first under 60 columns).
- **Day cursor:** a clicked bar (or the wheel) reads out over the chart captions (`Thu 15 Oct (UTC) · 34 opened · 17
  merged ✕`), never in place of the 24 h block; the rule keeps the scope and `counted 2m ago`. (The mock's `Wed 15 Oct` was
  a Thursday.)
- **Short terminals:** "eight list rows" is read as eight PR rows plus the table's rule and header, so the panel needs
  `rows ≥ 3 + panel_h + 2 + 8` (Cozy from 20 rows, Comfortable from 22). The §4 formula without the table chrome contradicted
  the §7 test ("a 20-row terminal auto-hides it").
- **`repo:` narrows the scope.** GitHub ORs `repo:` with `user:` / `org:`, so `org:acme repo:acme/app` counts all of acme.
  `stats::scope_of` drops owner qualifiers when a `repo:` is present. **The PR list has the same problem** (All PRs or an
  org scope plus the repo chip lists the union) — not fixed here.
- **Requests:** two per refresh as designed (merged days + 6 window aliases = 20; created days = 14), concurrent, each
  `rateLimit.cost` 1. Measured with `gh` (incl. ~0.5 s overhead): org 2.2–3.7 s, All PRs (user + 6 orgs) 1.9–3.1 s.
  In-app, All PRs created landed ~2.1 s after the scope switch and merged ~1–2 s later.
- **Fetch gating:** `App.stats.shown` is set by `ui::draw` each frame; nothing is fetched until the panel has been drawn,
  so hidden, too narrow, too short, the composer and the palette all pause it. A rejected answer for an old scope is dropped
  by a generation counter. `refresh_secs = 0` means "only on `r`".
- **Scope switching** happens in `App::handle` (`sync_stats` after every message), so a chip change repaints from that
  scope's cache at once; the fetch follows on the next tick.
- **Palette:** commands rank after PR matches, and `stats:` parses as a qualifier, so type `stats follow` (not `stats:`).
- **Header chip** is green and underlined when on, dim when off. The Compact row drops the `▂▅` glyph for room.
- Not built (per §8): double-click filtering, `exclude_bots`, a days picker, pixel bars, weekday ticks.
