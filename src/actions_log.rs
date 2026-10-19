//! GitHub Actions job logs: parsing, step mapping, folding, search and ANSI styling. Pure, no I/O.

use std::collections::HashSet;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use crate::github::Step;

/// Most lines kept from the end of a log; older ones are dropped.
pub const MAX_LINES: usize = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Group,
    EndGroup,
    Error,
    Warning,
    Notice,
    Debug,
    Command,
    Section,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    /// Unix seconds from the line's timestamp prefix.
    pub ts: Option<i64>,
    pub kind: Kind,
    /// Text after the timestamp and `##[…]` marker, ANSI kept.
    pub text: String,
    /// `text` without ANSI, for search and width.
    pub plain: String,
    pub group: Option<usize>,
    /// Index into the job's steps, from the timestamp.
    pub step: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Group {
    /// Line index of the `##[group]` header.
    pub start: usize,
    /// Last line index inside the group (the header when it's empty).
    pub end: usize,
    pub title: String,
    pub has_error: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedLog {
    pub lines: Vec<LogLine>,
    pub groups: Vec<Group>,
    /// Line indices of `##[error]` / `##[warning]` rows.
    pub errors: Vec<usize>,
    pub warnings: Vec<usize>,
    /// Lines dropped from the front to stay under the caps.
    pub dropped_lines: usize,
    /// First line of each step (indexed like the job's steps), set by `assign_steps`.
    pub step_first: Vec<Option<usize>>,
}

impl ParsedLog {
    /// Lines worth counting: everything but the `##[endgroup]` markers.
    pub fn shown_len(&self) -> usize {
        self.lines.iter().filter(|l| l.kind != Kind::EndGroup).count()
    }
}

/// Split `2026-10-13T08:36:27.4323154Z text` into (seconds, text).
fn split_ts(line: &str) -> (Option<i64>, &str) {
    if let Some((head, rest)) = line.split_once(' ')
        && head.len() >= 20
        && head.ends_with('Z')
        && head.as_bytes()[10] == b'T'
        && let Some(ts) = crate::ui::parse_ts(head)
    {
        return (Some(ts), rest);
    }
    (None, line)
}

fn split_marker(text: &str) -> (Kind, &str) {
    const MARKERS: [(&str, Kind); 8] = [
        ("##[group]", Kind::Group),
        ("##[endgroup]", Kind::EndGroup),
        ("##[error]", Kind::Error),
        ("##[warning]", Kind::Warning),
        ("##[notice]", Kind::Notice),
        ("##[debug]", Kind::Debug),
        ("##[command]", Kind::Command),
        ("##[section]", Kind::Section),
    ];
    for (m, k) in MARKERS {
        if let Some(rest) = text.strip_prefix(m) {
            return (k, rest);
        }
    }
    (Kind::Plain, text)
}

pub fn parse(text: &str) -> ParsedLog {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut raw: Vec<&str> = text.lines().collect();
    let mut dropped_lines = 0;
    if raw.len() > MAX_LINES {
        dropped_lines = raw.len() - MAX_LINES;
        raw.drain(..dropped_lines);
    }
    let mut log = ParsedLog { dropped_lines, ..Default::default() };
    let mut open: Option<usize> = None;
    for line in raw {
        let (ts, rest) = split_ts(line.strip_prefix('\u{feff}').unwrap_or(line));
        let (mut kind, body) = split_marker(rest);
        let idx = log.lines.len();
        let mut group = open;
        match kind {
            Kind::Group => {
                // Groups don't nest: a new header closes the open one.
                log.groups.push(Group { start: idx, end: idx, title: strip_ansi(body), has_error: false });
                open = Some(log.groups.len() - 1);
                group = open;
            }
            Kind::EndGroup if open.is_some() => open = None,
            // A stray end marker means nothing; show it as an ordinary (empty) line.
            Kind::EndGroup => kind = Kind::Plain,
            Kind::Error => log.errors.push(idx),
            Kind::Warning => log.warnings.push(idx),
            _ => {}
        }
        if let Some(g) = group {
            let grp = &mut log.groups[g];
            grp.end = idx;
            grp.has_error |= kind == Kind::Error;
        }
        log.lines.push(LogLine { ts, kind, text: body.to_string(), plain: strip_ansi(body), group, step: None });
    }
    log
}

/// Attach each line to the step whose time window contains its timestamp.
/// Lines without a timestamp inherit the previous line's step.
pub fn assign_steps(log: &mut ParsedLog, steps: &[Step]) {
    let windows: Vec<(usize, i64, Option<i64>)> = steps
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let start = crate::ui::parse_ts(s.started_at.as_deref()?)?;
            Some((i, start, s.completed_at.as_deref().and_then(crate::ui::parse_ts)))
        })
        .collect();
    let mut k: Option<usize> = None;
    log.step_first = vec![None; steps.len()];
    for (idx, line) in log.lines.iter_mut().enumerate() {
        let Some(ts) = line.ts else {
            line.step = k.map(|k| windows[k].0);
            continue;
        };
        loop {
            let next = k.map_or(0, |k| k + 1);
            let Some(&(_, nstart, _)) = windows.get(next) else { break };
            if nstart > ts {
                break;
            }
            // The current step is over, or a new command starts in the next one's window.
            let over = k.is_none_or(|k| windows[k].2.is_some_and(|end| ts > end));
            if over || line.kind == Kind::Group {
                k = Some(next);
            } else {
                break;
            }
        }
        line.step = k.map(|k| windows[k].0);
        if let Some(s) = line.step
            && log.step_first[s].is_none()
        {
            log.step_first[s] = Some(idx);
        }
    }
}

/// Keep at most the last `cap` bytes, cut at a line boundary. Returns the kept bytes and
/// the number of whole lines dropped.
pub fn tail(bytes: &[u8], cap: usize) -> (&[u8], usize) {
    if bytes.len() <= cap {
        return (bytes, 0);
    }
    let cut = bytes.len() - cap;
    // Start after the first newline at or past the cut so no line is kept partially.
    let start = match bytes[cut.saturating_sub(1)..].iter().position(|&b| b == b'\n') {
        Some(p) => cut.saturating_sub(1) + p + 1,
        None => bytes.len(),
    };
    let dropped = bytes[..start].iter().filter(|&&b| b == b'\n').count();
    (&bytes[start..], dropped)
}

/// `tail` over a stream: holds at most twice `cap` bytes while chunks arrive, so a huge log
/// is never buffered whole. The result is at most `cap` bytes, cut at a line boundary.
pub struct Tail {
    buf: Vec<u8>,
    cap: usize,
    dropped: usize,
    /// A line was cut before its newline arrived; discard up to that newline.
    skipping: bool,
}

impl Tail {
    pub fn new(cap: usize) -> Self {
        Tail { buf: Vec::new(), cap, dropped: 0, skipping: false }
    }

    pub fn push(&mut self, mut chunk: &[u8]) {
        if self.skipping {
            let Some(p) = chunk.iter().position(|&b| b == b'\n') else { return };
            self.skipping = false;
            self.dropped += 1;
            chunk = &chunk[p + 1..];
        }
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > self.cap.saturating_mul(2) {
            let (kept, dropped) = tail(&self.buf, self.cap);
            let start = self.buf.len() - kept.len();
            self.skipping = start == self.buf.len() && self.buf.last() != Some(&b'\n');
            self.dropped += dropped;
            self.buf.drain(..start);
        }
    }

    /// The kept bytes and the number of whole lines dropped.
    pub fn finish(self) -> (Vec<u8>, usize) {
        let (kept, dropped) = tail(&self.buf, self.cap);
        (kept.to_vec(), self.dropped + dropped)
    }
}

/// Groups folded when a log is first shown: all but the first failing one and the last.
pub fn default_folds(log: &ParsedLog) -> HashSet<usize> {
    let first_err = log.groups.iter().position(|g| g.has_error);
    let last = log.groups.len().checked_sub(1);
    (0..log.groups.len()).filter(|g| Some(*g) != first_err && Some(*g) != last).collect()
}

/// Indices of the lines on screen: folded groups show only their header.
pub fn visible_lines(log: &ParsedLog, folds: &HashSet<usize>) -> Vec<usize> {
    log.lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.kind != Kind::EndGroup)
        .filter(|(_, l)| l.kind == Kind::Group || l.group.is_none_or(|g| !folds.contains(&g)))
        .map(|(i, _)| i)
        .collect()
}

/// Lines whose text contains `query`, ignoring case and ANSI.
pub fn search(log: &ParsedLog, query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    log.lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.kind != Kind::EndGroup && l.plain.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

/// Remove every escape sequence (CSI, OSC and lone ESC pairs) and other control characters.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            skip_escape(&mut it);
        } else if c == '\t' {
            out.push_str("    ");
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// Consume an escape after the ESC; returns the CSI parameters and final byte when it was CSI.
fn skip_escape(it: &mut std::iter::Peekable<std::str::Chars>) -> Option<(String, char)> {
    match it.next()? {
        '[' => {
            let mut params = String::new();
            for c in it.by_ref() {
                if ('\x40'..='\x7e').contains(&c) {
                    return Some((params, c));
                }
                params.push(c);
            }
            None
        }
        ']' => {
            // OSC runs to BEL or ESC \.
            while let Some(c) = it.next() {
                if c == '\x07' {
                    break;
                }
                if c == '\x1b' {
                    if it.peek() == Some(&'\\') {
                        it.next();
                    }
                    break;
                }
            }
            None
        }
        _ => None,
    }
}

fn basic_color(n: u16) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        _ => Color::White,
    }
}

/// Apply one SGR parameter list to `style`.
fn apply_sgr(params: &str, base: Style, style: Style) -> Style {
    let nums: Vec<u16> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
    let mut s = style;
    let mut i = 0;
    while i < nums.len() {
        match nums[i] {
            0 => s = base,
            1 => s = s.add_modifier(Modifier::BOLD),
            2 => s = s.add_modifier(Modifier::DIM),
            3 => s = s.add_modifier(Modifier::ITALIC),
            4 => s = s.add_modifier(Modifier::UNDERLINED),
            22 => s = s.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => s = s.remove_modifier(Modifier::ITALIC),
            24 => s = s.remove_modifier(Modifier::UNDERLINED),
            n @ 30..=37 => s = s.fg(basic_color(n - 30)),
            39 => s.fg = base.fg,
            n @ 90..=97 => s = s.fg(basic_color(n - 90 + 8)),
            38 if nums.get(i + 1) == Some(&5) => {
                if let Some(&n) = nums.get(i + 2) {
                    s = s.fg(Color::Indexed(n as u8));
                }
                i += 2;
            }
            38 if nums.get(i + 1) == Some(&2) => {
                if let (Some(&r), Some(&g), Some(&b)) = (nums.get(i + 2), nums.get(i + 3), nums.get(i + 4)) {
                    s = s.fg(Color::Rgb(r as u8, g as u8, b as u8));
                }
                i += 4;
            }
            _ => {}
        }
        i += 1;
    }
    s
}

/// Styled spans from text with ANSI SGR codes; every other escape is dropped.
pub fn ansi_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut style = base;
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            if let Some((params, 'm')) = skip_escape(&mut it) {
                if !buf.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buf), style));
                }
                style = apply_sgr(&params, base, style);
            }
        } else if c == '\t' {
            buf.push_str("    ");
        } else if !c.is_control() {
            buf.push(c);
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(start: &str, end: Option<&str>) -> Step {
        Step {
            name: String::new(),
            number: 0,
            status: "completed".into(),
            conclusion: None,
            started_at: Some(start.into()),
            completed_at: end.map(String::from),
        }
    }

    #[test]
    fn strips_bom_and_timestamps() {
        let log = parse("\u{feff}2026-10-13T08:36:27.4323154Z hello\nno stamp");
        assert_eq!(log.lines[0].text, "hello");
        assert_eq!(log.lines[0].ts, crate::ui::parse_ts("2026-10-13T08:36:27Z"));
        assert_eq!(log.lines[1].ts, None);
        assert_eq!(log.lines[1].text, "no stamp");
    }

    #[test]
    fn pairs_groups() {
        let text = "##[group]Run a\nx\n##[endgroup]\n##[endgroup]\n##[group]Run b\n##[error]boom\ny";
        let log = parse(text);
        assert_eq!(log.groups.len(), 2);
        assert_eq!((log.groups[0].start, log.groups[0].end), (0, 2));
        assert_eq!(log.groups[0].title, "Run a");
        // The stray end marker is a plain line outside any group.
        assert_eq!(log.lines[3].kind, Kind::Plain);
        assert_eq!(log.lines[3].group, None);
        // The unterminated group runs to the end.
        assert_eq!(log.groups[1].end, 6);
        assert!(log.groups[1].has_error && !log.groups[0].has_error);
        assert_eq!(log.lines[5].kind, Kind::Error);
        assert_eq!(log.errors, vec![5]);
    }

    #[test]
    fn styles_ansi() {
        let spans = ansi_spans("\x1b[36;1mcmd\x1b[0m rest", Style::new());
        assert_eq!(spans[0].content, "cmd");
        assert_eq!(spans[0].style.fg, Some(Color::Cyan));
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[1].content, " rest");
        assert_eq!(spans[1].style, Style::new());
        assert_eq!(ansi_spans("\x1b[92mok", Style::new())[0].style.fg, Some(Color::LightGreen));
        assert_eq!(ansi_spans("\x1b[38;5;196mred", Style::new())[0].style.fg, Some(Color::Indexed(196)));
        assert_eq!(ansi_spans("\x1b[38;2;1;2;3mrgb", Style::new())[0].style.fg, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn strips_other_escapes() {
        assert_eq!(strip_ansi("a\x1b[2Kb"), "ab");
        assert_eq!(strip_ansi("\x1b]8;;https://x.y\x1b\\link\x1b]8;;\x1b\\"), "link");
        assert_eq!(strip_ansi("end\x1b["), "end");
        let spans = ansi_spans("a\x1b[2Kb\x1b]8;;u\x07c", Style::new());
        assert_eq!(spans.iter().map(|s| s.content.as_ref()).collect::<String>(), "abc");
    }

    #[test]
    fn tail_keeps_whole_lines() {
        let (kept, dropped) = tail(b"one\ntwo\nthree\n", 8);
        assert_eq!(kept, b"three\n");
        assert_eq!(dropped, 2);
        assert_eq!(tail(b"ab\ncd\n", 100), (&b"ab\ncd\n"[..], 0));
        // A cut exactly at a line start keeps that line.
        assert_eq!(tail(b"ab\ncd\n", 3), (&b"cd\n"[..], 1));
    }

    #[test]
    fn tail_streams_within_its_cap() {
        let text: String = (0..500).map(|i| format!("line {i}\n")).collect();
        for chunk in [1, 7, 64, 1000] {
            let mut t = Tail::new(100);
            for c in text.as_bytes().chunks(chunk) {
                t.push(c);
                assert!(t.buf.len() <= 200 + chunk);
            }
            let (kept, dropped) = t.finish();
            assert!(kept.len() <= 100 && text.ends_with(std::str::from_utf8(&kept).unwrap()));
            assert_eq!(dropped + kept.iter().filter(|&&b| b == b'\n').count(), 500);
            assert!(kept.starts_with(b"line "));
        }
        // A line longer than the cap is dropped whole, even split across chunks.
        let mut t = Tail::new(4);
        for c in [&b"ab\n"[..], b"0123456789", b"0123\nxy\n"] {
            t.push(c);
        }
        assert_eq!(t.finish(), (b"xy\n".to_vec(), 2));
    }

    #[test]
    fn assigns_steps_by_time() {
        let text = "2026-10-13T08:00:00.1Z before\n\
                    2026-10-13T08:00:01.5Z ##[group]Run one\n\
                    no timestamp\n\
                    2026-10-13T08:00:05.0Z ##[group]Run two\n\
                    2026-10-13T08:09:00.0Z still running";
        let mut log = parse(text);
        let steps = [step("2026-10-13T08:00:01Z", Some("2026-10-13T08:00:05Z")), step("2026-10-13T08:00:05Z", None)];
        assign_steps(&mut log, &steps);
        let got: Vec<Option<usize>> = log.lines.iter().map(|l| l.step).collect();
        assert_eq!(got, vec![None, Some(0), Some(0), Some(1), Some(1)]);
    }

    #[test]
    fn folded_group_shows_header_only() {
        let log = parse("top\n##[group]G\na\nb\n##[endgroup]\nafter");
        let folds: HashSet<usize> = [0].into();
        assert_eq!(visible_lines(&log, &folds), vec![0, 1, 5]);
        assert_eq!(visible_lines(&log, &HashSet::new()), vec![0, 1, 2, 3, 5]);
    }

    #[test]
    fn default_folds_keep_error_and_last() {
        let log = parse("##[group]a\n##[group]b\n##[error]x\n##[group]c\n##[group]d");
        assert_eq!(default_folds(&log), [0, 2].into());
    }

    #[test]
    fn search_ignores_ansi_and_case() {
        let log = parse("\x1b[31mFAILED\x1b[0m test\nok\nfai\x1b[1mled");
        assert_eq!(search(&log, "failed"), vec![0, 2]);
        assert!(search(&log, "").is_empty());
    }
}
