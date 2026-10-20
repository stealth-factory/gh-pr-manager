//! The stats panel's arithmetic (docs/stats-panel.md): UTC calendar days, the aliased count
//! queries and their answers, eighth-block bars, the big half-block digits and the panel's
//! layout at every width. Pure: no I/O and no clock reads beyond what callers pass in.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::Density;

/// Days on the axis: `[stats] days` is clamped to this range.
pub const MIN_DAYS: usize = 7;
pub const MAX_DAYS: usize = 30;
/// List rows the panel always leaves the table; a shorter terminal hides it.
pub const MIN_LIST_ROWS: u16 = 8;
/// Rows of the table that are not PR rows (the rule above it and its header).
pub const LIST_CHROME: u16 = 2;
/// Cells to the left of the LAST 24H block.
pub const MARGIN: u16 = 2;
/// Cells between the figures and the first chart, and between the two charts.
pub const GUTTER: u16 = 4;
/// Room for three digits drawn large (`4n - 1` cells). Fixed so the column never jumps.
pub const FIGURE_DIGITS: u16 = 11;
/// Cells between two figure slots.
pub const FIGURE_GAP: u16 = 2;
/// The three-figure LAST 24H block.
pub const FIGURE_W: u16 = 3 * FIGURE_DIGITS + 2 * FIGURE_GAP;
/// Narrower than this and the one-row form is used at any density (the big figures need it).
pub const FULL_FROM: u16 = MARGIN + FIGURE_W + 1;
/// Narrower than this and the panel is not drawn at all.
pub const SPARK_FROM: u16 = 24;

const EIGHTHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

// ---------- dates (UTC, civil-from-days; no chrono) ----------

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's `civil_from_days`.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// The UTC day `secs` (unix seconds) falls on, as days since the epoch.
pub fn day_of(secs: i64) -> i64 {
    secs.div_euclid(86_400)
}

/// `YYYY-MM-DD` of a day number.
pub fn date_string(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DDTHH:MM:SSZ` of unix seconds: the rolling 24 h cut GitHub's search accepts.
pub fn iso_datetime(secs: i64) -> String {
    let rem = secs.rem_euclid(86_400);
    format!("{}T{:02}:{:02}:{:02}Z", date_string(day_of(secs)), rem / 3600, rem % 3600 / 60, rem % 60)
}

/// `Wed 15 Oct` for the day readout.
pub fn day_label(day: i64) -> String {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (_, m, d) = civil_from_days(day);
    // 1970-01-01 was a Thursday.
    format!("{} {d} {}", WEEKDAYS[(day + 4).rem_euclid(7) as usize], MONTHS[m as usize - 1])
}

/// The `n` UTC days ending with `today` (oldest first), as date strings. Today is partial.
pub fn chart_days(today: i64, n: usize) -> Vec<String> {
    (0..n as i64).rev().map(|k| date_string(today - k)).collect()
}

/// Inverse of `date_string`, for the day readout.
pub fn parse_day(s: &str) -> Option<i64> {
    crate::ui::parse_ts(&format!("{s}T00:00:00Z")).map(day_of)
}

// ---------- queries ----------

/// The scope the figures describe: the list's query minus everything that is not "which PRs":
/// `is:pr`, `archived:false`, the state filter and `sort:`. Merged/day needs merged PRs
/// whatever the state chip says.
///
/// GitHub ORs `repo:`, `user:` and `org:` together, so `org:acme repo:acme/app` counts all of
/// acme: with a `repo:` the owner qualifiers are dropped, which is what the repo chip means.
pub fn scope_of(search_query: &str) -> String {
    const DROP: [&str; 7] = ["is:pr", "archived:false", "is:open", "is:closed", "is:merged", "is:unmerged", "is:pull-request"];
    let lower = |t: &str| t.to_ascii_lowercase();
    let has_repo = search_query.split_whitespace().any(|t| lower(t).starts_with("repo:"));
    search_query
        .split_whitespace()
        .filter(|t| !DROP.iter().any(|d| t.eq_ignore_ascii_case(d)) && !lower(t).starts_with("sort:"))
        .filter(|t| !(has_repo && (lower(t).starts_with("user:") || lower(t).starts_with("org:"))))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `review-requested:` loses a PR once it is merged (no pending request is left), so merges are
/// not countable in that scope.
pub fn merges_countable(scope: &str) -> bool {
    !scope.split_whitespace().any(|t| t.to_ascii_lowercase().starts_with("review-requested:"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// `m0…` merged per day, plus the three 24 h windows.
    Merged,
    /// `c0…` created per day.
    Created,
}

impl Which {
    pub fn index(self) -> usize {
        match self {
            Which::Merged => 0,
            Which::Created => 1,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Which::Merged => "m",
            Which::Created => "c",
        }
    }
}

/// One request's aliased `search { issueCount }`: a day each (`merged:` / `created:` on a bare
/// UTC date), and with `Merged` the 24 h windows opened / merged / closed (a full datetime cut,
/// so "the last 24 hours" rather than "today so far") and the same three for the 24 h before
/// (`before..since`, for the deltas). One rate-limit point whatever the count.
pub fn count_query(scope: &str, days: &[String], since: &str, before: &str, which: Which) -> String {
    let lit = |q: String| serde_json::to_string(&q).unwrap_or_else(|_| "\"\"".into());
    let one = |alias: &str, q: String| format!(" {alias}: search(query:{}, type:ISSUE) {{ issueCount }}", lit(q));
    let mut out = String::from("query {");
    for (n, day) in days.iter().enumerate() {
        let q = match which {
            Which::Merged => format!("{scope} is:pr is:merged merged:{day}"),
            Which::Created => format!("{scope} is:pr created:{day}"),
        };
        out.push_str(&one(&format!("{}{n}", which.prefix()), q));
    }
    if which == Which::Merged {
        out.push_str(&one("mw", format!("{scope} is:pr is:merged merged:>={since}")));
        out.push_str(&one("cw", format!("{scope} is:pr created:>={since}")));
        out.push_str(&one("xw", format!("{scope} is:pr is:closed is:unmerged closed:>={since}")));
        out.push_str(&one("mp", format!("{scope} is:pr is:merged merged:{before}..{since}")));
        out.push_str(&one("cp", format!("{scope} is:pr created:{before}..{since}")));
        out.push_str(&one("xp", format!("{scope} is:pr is:closed is:unmerged closed:{before}..{since}")));
    }
    out.push_str(" rateLimit { cost remaining limit } }");
    out
}

fn alias(v: &Value, name: &str) -> Result<i64, String> {
    v.get(name).and_then(|a| a.get("issueCount")).and_then(Value::as_i64).ok_or_else(|| format!("GitHub left {name} out of the answer"))
}

/// The per-day counts of an answer, by date. Deliberately not `unwrap_or(0)`: an alias GitHub did
/// not answer is an unknown, and a zero drawn in its place claims a quiet day.
pub fn read_series(v: &Value, days: &[String], which: Which) -> Result<Vec<(String, i64)>, String> {
    days.iter().enumerate().map(|(n, d)| Ok((d.clone(), alias(v, &format!("{}{n}", which.prefix()))?))).collect()
}

/// The rolling 24 h figures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub opened: i64,
    pub merged: i64,
    pub closed: i64,
}

pub fn read_window(v: &Value) -> Result<Window, String> {
    Ok(Window { opened: alias(v, "cw")?, merged: alias(v, "mw")?, closed: alias(v, "xw")? })
}

/// The 24 h before the window; `None` (no deltas drawn) when any of it is missing.
pub fn read_prev_window(v: &Value) -> Option<Window> {
    Some(Window { opened: alias(v, "cp").ok()?, merged: alias(v, "mp").ok()?, closed: alias(v, "xp").ok()? })
}

/// `▲3`, `▼12` or `=` against the 24 h before.
pub fn delta(now: i64, before: i64) -> String {
    match now - before {
        0 => "=".into(),
        d if d > 0 => format!("▲{d}"),
        d => format!("▼{}", -d),
    }
}

/// Every axis day's figure, or nothing: a day the answer does not carry is not a day with
/// nothing in it. Looked up by date string, never by position, so counts fetched before
/// midnight cannot slide one day along.
#[cfg(test)]
pub fn series_for(days: &[String], have: &[(String, i64)]) -> Option<Vec<(String, i64)>> {
    days.iter().map(|d| have.iter().find(|(h, _)| h == d).map(|(_, v)| (d.clone(), *v))).collect()
}

/// Each axis day's figure where known (the panel draws a known day and dims an unknown one).
pub fn lookup(days: &[String], have: &[(String, i64)]) -> Vec<Option<i64>> {
    days.iter().map(|d| have.iter().find(|(h, _)| h == d).map(|(_, v)| *v)).collect()
}

/// (total, peak) over the known days.
pub fn totals(values: &[Option<i64>]) -> (i64, i64) {
    let known = values.iter().flatten();
    (known.clone().sum(), known.copied().max().unwrap_or(0))
}

// ---------- drawing ----------

/// Vertical bars, one column per value, `height` rows of eighth blocks, linear against `peak`
/// (0 → the largest value), top row first. As opscope's `vbars`.
pub fn bars(values: &[f64], height: usize, peak: f64) -> Vec<Vec<char>> {
    let hi = if peak > 0.0 { peak } else { values.iter().copied().fold(0.0, f64::max).max(1.0) };
    (0..height)
        .map(|r| {
            let top = hi * (height - r) as f64 / height as f64;
            let bottom = hi * (height - r - 1) as f64 / height as f64;
            values
                .iter()
                .map(|&v| {
                    if v >= top {
                        '█'
                    } else if v <= bottom {
                        ' '
                    } else {
                        EIGHTHS[(((v - bottom) / (top - bottom) * 8.0) as usize).clamp(1, 8)]
                    }
                })
                .collect()
        })
        .collect()
}

/// One sparkline glyph per value (the Compact row).
pub fn spark(values: &[Option<i64>], peak: i64) -> String {
    values
        .iter()
        .map(|v| match v {
            None => '·',
            Some(0) => ' ',
            Some(v) => EIGHTHS[((*v as f64 / peak.max(1) as f64 * 8.0).ceil() as usize).clamp(1, 8)],
        })
        .collect()
}

const DIGITS: [[&str; 5]; 10] = [
    ["###", "# #", "# #", "# #", "###"],
    ["  #", "  #", "  #", "  #", "  #"],
    ["###", "  #", "###", "#  ", "###"],
    ["###", "  #", "###", "  #", "###"],
    ["# #", "# #", "###", "  #", "  #"],
    ["###", "#  ", "###", "  #", "###"],
    ["###", "#  ", "###", "# #", "###"],
    ["###", "  #", "  #", "  #", "  #"],
    ["###", "# #", "###", "# #", "###"],
    ["###", "# #", "###", "  #", "###"],
];

/// A number three text rows tall: a 3×5 pixel font on half blocks, two pixel rows to a cell,
/// digits one cell apart. Every row is the same width (`4n - 1`).
pub fn big_digits(value: i64) -> [String; 3] {
    let mut rows: [String; 3] = Default::default();
    for (i, ch) in value.to_string().chars().enumerate() {
        let glyph = ch.to_digit(10).map(|d| DIGITS[d as usize]).unwrap_or(["   "; 5]);
        for (r, row) in rows.iter_mut().enumerate() {
            if i > 0 {
                row.push(' ');
            }
            for c in 0..3 {
                // The sixth pixel row does not exist: the gap under the figure.
                let lit = |line: Option<&&str>| line.and_then(|l| l.as_bytes().get(c)).copied() == Some(b'#');
                row.push(match (lit(glyph.get(r * 2)), lit(glyph.get(r * 2 + 1))) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                });
            }
        }
    }
    rows
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Two bar charts side by side (and the big figures when `figures` is set in the layout).
    Full,
    /// One row of sparklines.
    Spark,
    /// Too narrow to draw anything honest.
    Hidden,
}

/// Where everything in the panel goes, in cells from its left edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub mode: Mode,
    pub days: usize,
    /// Cells given to each chart; 0 when only the figures fit.
    pub chart_w: u16,
    /// Cells per day, of which `barw` are bar and `gap` blank.
    pub slot: u16,
    pub gap: u16,
    pub barw: u16,
    /// Cells the bars (and the baseline) take.
    pub span: u16,
    /// Left edge of the opened and the merged chart.
    pub charts: [u16; 2],
    /// Left edge of the LAST 24H block (drawn large whenever the mode is `Full`).
    pub figures: Option<u16>,
}

impl Layout {
    pub fn fit(width: u16, days: usize, density: Density) -> Layout {
        let days = days.clamp(1, MAX_DAYS);
        let other = |mode| Layout { mode, days, chart_w: 0, slot: 0, gap: 0, barw: 0, span: 0, charts: [0, 0], figures: None };
        if width < SPARK_FROM {
            return other(Mode::Hidden);
        }
        if density == Density::Compact || width < FULL_FROM {
            return other(Mode::Spark);
        }
        // The LAST 24H block is the hero: first, and kept at full size. The 14-day charts share
        // what is left and go first when a cell a day no longer fits.
        let from = MARGIN + FIGURE_W + GUTTER;
        let chart_w = width.saturating_sub(from + GUTTER + 1) / 2;
        if (chart_w as usize) < days {
            return Layout { figures: Some(MARGIN), ..other(Mode::Full) };
        }
        let slot = chart_w / days as u16;
        let gap = u16::from(slot >= 3);
        let barw = slot - gap;
        let span = barw * days as u16 + gap * (days as u16 - 1);
        Layout {
            mode: Mode::Full,
            days,
            chart_w,
            slot,
            gap,
            barw,
            span,
            charts: [from, from + chart_w + GUTTER],
            figures: Some(MARGIN),
        }
    }

    /// Whether the two 14-day charts are drawn beside the figures.
    pub fn has_charts(&self) -> bool {
        self.mode == Mode::Full && self.span > 0
    }

    /// Left edge of day `i`'s bar in chart `c`.
    pub fn bar_x(&self, c: usize, i: usize) -> u16 {
        self.charts[c] + i as u16 * self.slot
    }

    /// The day under column `x` (from the panel's left edge), in either chart: the inverse of
    /// `bar_x`, which places the bars and their click targets.
    #[cfg(test)]
    pub fn day_at(&self, x: u16) -> Option<usize> {
        if !self.has_charts() {
            return None;
        }
        self.charts.iter().find_map(|&c| {
            let rel = x.checked_sub(c)?;
            (rel < self.span).then(|| (rel / self.slot) as usize).filter(|&i| i < self.days)
        })
    }
}

/// Rows the panel takes.
pub fn panel_height(density: Density, mode: Mode) -> u16 {
    match (mode, density) {
        (Mode::Hidden, _) => 0,
        (Mode::Spark, _) | (_, Density::Compact) => 1,
        (Mode::Full, Density::Comfortable) => 9,
        (Mode::Full, Density::Cozy) => 7,
    }
}

/// Whether a `rows`-tall terminal has room for a `panel_h` panel and still `MIN_LIST_ROWS` PRs
/// (header, status line and footer are a row each).
pub fn fits_height(rows: u16, panel_h: u16) -> bool {
    rows >= 3 + panel_h + LIST_CHROME + MIN_LIST_ROWS
}

/// `counted 3m ago` wording for a count `secs` old.
pub fn age(secs: i64) -> String {
    match secs.max(0) {
        0..60 => "just now".into(),
        s @ 60..3600 => format!("{}m ago", s / 60),
        s @ 3600..86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn day(y: i64, m: u32, d: u32) -> i64 {
        parse_day(&format!("{y:04}-{m:02}-{d:02}")).unwrap()
    }

    #[test]
    fn civil_dates_round_trip_and_cross_month_and_leap_ends() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        for z in -800_000..800_000 {
            if z % 997 == 0 {
                assert_eq!(parse_day(&date_string(z)), Some(z), "{z}");
            }
        }
        assert_eq!(chart_days(day(2026, 11, 2), 4), ["2026-10-30", "2026-10-31", "2026-11-01", "2026-11-02"]);
        assert_eq!(chart_days(day(2028, 3, 1), 3), ["2028-02-28", "2028-02-29", "2028-03-01"]);
        assert_eq!(chart_days(day(2027, 1, 1), 2), ["2026-12-31", "2027-01-01"]);
        assert_eq!(chart_days(day(2026, 10, 20), 14).len(), 14);
        assert_eq!(day_label(day(2026, 10, 15)), "Thu 15 Oct");
        assert_eq!(day_label(day(2028, 2, 29)), "Tue 29 Feb");
        // The 24 h cut is a full UTC datetime.
        let t = day(2026, 10, 19) * 86_400 + 4 * 3600 + 12 * 60 + 5;
        assert_eq!(iso_datetime(t), "2026-10-19T04:12:05Z");
        assert_eq!(date_string(day_of(t)), "2026-10-19");
        assert_eq!(date_string(day_of(day(2026, 10, 20) * 86_400 - 1)), "2026-10-19");
    }

    #[test]
    fn scope_strips_everything_but_which_prs() {
        assert_eq!(scope_of("is:pr archived:false is:open org:stealth-factory sort:updated-desc"), "org:stealth-factory");
        assert_eq!(scope_of("is:pr archived:false is:closed is:unmerged author:@me repo:a/b sort:updated-desc"), "author:@me repo:a/b");
        assert_eq!(scope_of("is:pr archived:false user:me org:a org:b sort:updated-desc"), "user:me org:a org:b");
        assert_eq!(scope_of("is:pr archived:false is:open user:me org:a repo:a/x sort:updated-desc"), "repo:a/x");
        assert!(!merges_countable("review-requested:@me"));
        assert!(merges_countable("author:@me repo:a/b"));
    }

    #[test]
    fn count_query_has_every_day_and_the_windows() {
        let days = chart_days(day(2026, 10, 20), 14);
        let since = "2026-10-19T04:12:00Z";
        let before = "2026-10-18T04:12:00Z";
        let q = count_query("org:stealth-factory", &days, since, before, Which::Merged);
        assert_eq!(q.matches("issueCount").count(), 14 + 6);
        assert!(q.contains(r#"cp: search(query:"org:stealth-factory is:pr created:2026-10-18T04:12:00Z..2026-10-19T04:12:00Z""#), "{q}");
        assert!(q.contains(r#"m0: search(query:"org:stealth-factory is:pr is:merged merged:2026-10-07", type:ISSUE)"#), "{q}");
        assert!(q.contains(r#"m13: search(query:"org:stealth-factory is:pr is:merged merged:2026-10-20""#));
        assert!(q.contains(r#"xw: search(query:"org:stealth-factory is:pr is:closed is:unmerged closed:>=2026-10-19T04:12:00Z""#));
        assert!(q.contains("rateLimit { cost remaining limit }"));
        let c = count_query("org:stealth-factory", &days, since, before, Which::Created);
        assert_eq!(c.matches("issueCount").count(), 14);
        assert!(c.contains(r#"c6: search(query:"org:stealth-factory is:pr created:2026-10-13""#));
        // Quotes in a scope are escaped, not closed.
        assert!(count_query(r#"label:"needs review""#, &days, since, before, Which::Created).contains(r#"label:\"needs review\" is:pr"#));
    }

    #[test]
    fn a_missing_alias_fails_the_series_never_reads_zero() {
        let days = chart_days(day(2026, 10, 20), 14);
        let counts = [3, 2, 37, 2, 30, 2, 29, 4, 1, 1, 1, 2, 2, 2];
        let mut v = json!({ "mw": { "issueCount": 8 }, "cw": { "issueCount": 14 }, "xw": { "issueCount": 0 } });
        for (n, c) in counts.iter().enumerate() {
            v[format!("m{n}")] = json!({ "issueCount": c });
        }
        let s = read_series(&v, &days, Which::Merged).unwrap();
        assert_eq!(s.len(), 14);
        assert_eq!(s[2], ("2026-10-09".to_string(), 37));
        assert_eq!(read_window(&v).unwrap(), Window { opened: 14, merged: 8, closed: 0 });
        assert_eq!(read_prev_window(&v), None);
        v["cp"] = json!({ "issueCount": 11 });
        v["mp"] = json!({ "issueCount": 8 });
        v["xp"] = json!({ "issueCount": 2 });
        assert_eq!(read_prev_window(&v), Some(Window { opened: 11, merged: 8, closed: 2 }));
        assert_eq!((delta(14, 11), delta(8, 8), delta(0, 2)), ("▲3".into(), "=".into(), "▼2".into()));
        v.as_object_mut().unwrap().remove("m5");
        assert!(read_series(&v, &days, Which::Merged).unwrap_err().contains("m5"));
        v.as_object_mut().unwrap().remove("xw");
        assert!(read_window(&v).is_err());
        assert!(read_series(&v, &days, Which::Created).is_err());
    }

    #[test]
    fn series_are_looked_up_by_date() {
        let have = vec![("2026-10-18".to_string(), 4), ("2026-10-19".to_string(), 7)];
        let days = vec!["2026-10-19".to_string(), "2026-10-20".to_string()];
        assert_eq!(series_for(&days, &have), None);
        assert_eq!(lookup(&days, &have), vec![Some(7), None]);
        assert_eq!(series_for(&days[..1], &have), Some(vec![("2026-10-19".to_string(), 7)]));
        assert_eq!(totals(&[Some(3), None, Some(9)]), (12, 9));
    }

    #[test]
    fn bars_scale_against_the_peak() {
        let b = bars(&[0.0, 8.0, 4.0, 1.0], 1, 8.0);
        assert_eq!(b, vec![vec![' ', '█', '▄', '▁']]);
        let b = bars(&[0.0, 30.0, 15.0], 3, 0.0);
        assert_eq!(b.len(), 3);
        assert_eq!(b.iter().map(|r| r[0]).collect::<String>(), "   ");
        assert_eq!(b.iter().map(|r| r[1]).collect::<String>(), "███");
        assert_eq!(b.iter().map(|r| r[2]).collect::<String>(), " ▄█");
        assert_eq!(spark(&[Some(0), Some(8), None, Some(1)], 8), " █·▁");
    }

    #[test]
    fn big_digits_are_three_rows_of_one_width() {
        assert_eq!(big_digits(0), ["█▀█", "█ █", "▀▀▀"]);
        for v in [1, 7, 42, 108, 9999] {
            let rows = big_digits(v);
            let w = rows[0].chars().count();
            assert_eq!(w, 4 * v.to_string().len() - 1);
            assert!(rows.iter().all(|r| r.chars().count() == w), "{v}: {rows:?}");
        }
        assert_eq!(big_digits(14)[0], "  █ █ █");
    }

    #[test]
    fn layout_keeps_the_figures_and_drops_the_charts_first() {
        for days in [7, 14, 30] {
            for w in [20, 23, 24, 39, 40, 44, 60, 64, 80, 99, 100, 120, 160, 200] {
                let l = Layout::fit(w, days, Density::Cozy);
                match l.mode {
                    Mode::Hidden => assert!(w < SPARK_FROM),
                    Mode::Spark => assert!((SPARK_FROM..FULL_FROM).contains(&w), "{days}d at {w}"),
                    Mode::Full => {
                        // The hero is always there, first, at full size.
                        assert_eq!(l.figures, Some(MARGIN));
                        assert!(MARGIN + FIGURE_W < w, "{days}d at {w}");
                        let room = w.saturating_sub(MARGIN + FIGURE_W + 2 * GUTTER + 1) / 2;
                        assert_eq!(l.has_charts(), room as usize >= days, "{days}d at {w}");
                        if !l.has_charts() {
                            assert_eq!(l.day_at(MARGIN + FIGURE_W + GUTTER), None);
                            continue;
                        }
                        assert!(l.charts[0] >= MARGIN + FIGURE_W + GUTTER, "{l:?}");
                        assert!(l.charts[0] + l.span <= l.charts[1], "{l:?}");
                        assert!(l.charts[1] + l.span < w, "{days}d at {w}: {l:?}");
                        assert_eq!(l.gap, u16::from(l.slot >= 3));
                        // Every bar round-trips through `day_at`, in both charts.
                        for c in 0..2 {
                            for i in 0..days {
                                for dx in 0..l.barw {
                                    assert_eq!(l.day_at(l.bar_x(c, i) + dx), Some(i), "{days}d at {w}");
                                }
                            }
                        }
                        assert_eq!(l.day_at(l.charts[0] - 1), None);
                        assert_eq!(l.day_at(l.charts[1] + l.span), None);
                    }
                }
            }
        }
        assert_eq!(Layout::fit(120, 14, Density::Cozy).slot, 2);
        assert_eq!(Layout::fit(23, 14, Density::Cozy).mode, Mode::Hidden);
        assert_eq!(Layout::fit(39, 14, Density::Cozy).mode, Mode::Spark);
        assert!(!Layout::fit(60, 14, Density::Cozy).has_charts());
        assert!(Layout::fit(80, 14, Density::Cozy).has_charts());
        assert_eq!(Layout::fit(200, 14, Density::Compact).mode, Mode::Spark);
        assert_eq!(panel_height(Density::Comfortable, Mode::Full), 9);
        assert_eq!(panel_height(Density::Cozy, Mode::Full), 7);
        assert_eq!(panel_height(Density::Cozy, Mode::Spark), 1);
        assert_eq!(panel_height(Density::Compact, Mode::Spark), 1);
        assert!(fits_height(20, 7) && !fits_height(20, 9) && !fits_height(19, 7));
    }
}
