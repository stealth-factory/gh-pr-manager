//! Rendering. Every clickable region is recorded into `app.hits` while drawing.

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, List, ListItem, Padding, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table,
};
use ratatui_image::sliced::{SignedPosition, SlicedImage};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::actions_log::{Kind, ParsedLog, ansi_spans};
use crate::app::{
    Action, App, ComposeKind, DetailRow, Focus, Hits, Item, LogView, Marks, Modal, RepoItem, Screen, Tab, ToastKind, TreeNode, repo_items,
    scope_matches, search_items,
};
use crate::changes::{Change, NewMarks, Snapshot};
use crate::search::Command;
use crate::state::Density;
use crate::github::{Annotation, FileDiff, Job, PrDetail, PrSummary, Step, TimelineEvent, WorkflowRunInfo, login_of};
use crate::stacks::{Member, Stack};
use crate::images::{self, ImagePlacement, ImageStore, Look};

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
/// `NEW` markers: the word carries the meaning, the colour sets them apart from the accent.
const NEW: Color = Color::Yellow;
/// The zero-width link on a `NEW` entry's header row (found by `n`/`p`, never clicked).
const NEW_LINK: &str = "#new";
/// The zero-width tag on the info header's `base ← head` row(s).
const ARROW_LINK: &str = "#arrow";
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

thread_local! {
    /// Density for the frame being drawn, read by the row builders' spacing helpers.
    static DENSITY: std::cell::Cell<Density> = const { std::cell::Cell::new(Density::Cozy) };
}

fn density() -> Density {
    DENSITY.with(|d| d.get())
}

/// Blank spacer rows between blocks: none when compact, two when comfortable.
fn gap(rows: &mut Vec<DetailRow>) {
    let n = match density() {
        Density::Comfortable => 2,
        Density::Cozy => 1,
        Density::Compact => 0,
    };
    for _ in 0..n {
        rows.push((Line::raw(""), Vec::new()));
    }
}

/// Left padding of detail text.
fn text_pad() -> u16 {
    match density() {
        Density::Comfortable => 2,
        Density::Cozy => 1,
        Density::Compact => 0,
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    app.hits = Hits::default();
    DENSITY.with(|d| d.set(app.state.density));
    let [header, body, status, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1), Constraint::Length(1)]).areas(f.area());

    draw_header(f, app, header);
    match app.screen {
        Screen::List => draw_table(f, app, body),
        Screen::Detail => {
            let [side, main] = Layout::horizontal([Constraint::Ratio(1, 3), Constraint::Ratio(2, 3)]).areas(body);
            draw_sidebar(f, app, side);
            draw_detail(f, app, main);
        }
    }
    draw_status(f, app, status);
    draw_footer(f, app, footer);

    if app.modal.is_some() {
        dim(f);
    }
    match &app.modal {
        Some(Modal::Scopes { .. }) => draw_scopes(f, app),
        Some(Modal::Repos { .. }) => draw_repos(f, app),
        Some(Modal::Menu { .. }) => draw_menu(f, app),
        Some(Modal::Confirm { .. }) => draw_confirm(f, app),
        Some(Modal::Composer(_)) => draw_composer(f, app),
        Some(Modal::Help) => draw_help(f, app),
        Some(Modal::Image { .. }) => draw_image_modal(f, app),
        Some(Modal::Search) => draw_search(f, app),
        None => {}
    }
    sanitize(f.buffer_mut());
}

/// Fade everything drawn so far, so a modal on top stands out.
fn dim(f: &mut Frame) {
    for cell in f.buffer_mut().content.iter_mut() {
        cell.fg = Color::Indexed(240);
        if cell.bg != Color::Reset {
            cell.bg = Color::Indexed(235);
        }
        cell.modifier = Modifier::empty();
    }
}

/// Make every cell's width unambiguous. Emoji presentation selectors (U+FE0F) and ZWJ
/// sequences make some terminals draw a glyph narrower than ratatui assumed, and the cell
/// ratatui then skips keeps stale content on screen. Image cells are left alone: Sixel and
/// iTerm2 put a whole escape sequence in one cell, Kitty a U+10EEEE placeholder.
fn sanitize(buf: &mut Buffer) {
    for cell in buf.content.iter_mut() {
        let sym = cell.symbol();
        if sym.starts_with('\u{1b}') || sym.contains('\u{10EEEE}') {
            continue;
        }
        let odd = |c: char| c == '\u{FE0F}' || c == '\u{FE0E}' || c == '\u{200D}' || c.is_control();
        if !sym.chars().any(odd) {
            continue;
        }
        let first = sym.chars().find(|c| !odd(*c)).unwrap_or(' ');
        let first = if first.width().unwrap_or(0) == 0 { ' ' } else { first };
        cell.set_char(first);
    }
}

// ---------- generic helpers ----------

/// Render clickable `[key label]` buttons left-to-right, recording their rects.
fn button_row(f: &mut Frame, hits: &mut Hits, area: Rect, items: &[(&str, &str, Action)], key_style: Style) {
    let mut x = area.x;
    let mut spans = Vec::new();
    for (key, label, action) in items {
        let text_w = key.width() + label.width() + if key.is_empty() { 2 } else { 3 };
        if x + text_w as u16 > area.right() {
            break;
        }
        hits.buttons.push((Rect::new(x, area.y, text_w as u16, 1), action.clone()));
        spans.push(Span::raw(" "));
        if !key.is_empty() {
            spans.push(Span::styled(key.to_string(), key_style));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::raw(label.to_string()));
        spans.push(Span::raw(" "));
        spans.push(Span::raw(" "));
        x += text_w as u16 + 1;
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2)).max(10.min(area.width));
    let h = h.min(area.height.saturating_sub(2)).max(3.min(area.height));
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn modal_block(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(format!(" {title} "), Style::new().bold()))
}

fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Word-wrap a styled line to `width`, prefixing continuation rows with `indent`.
fn wrap(line: Line<'static>, width: usize, indent: &str) -> Vec<Line<'static>> {
    let width = width.max(8);
    if line.width() <= width {
        return vec![line];
    }
    // Break the line into words (each may span several styled pieces); a word ends after whitespace.
    let mut words: Vec<Vec<(String, Style)>> = vec![Vec::new()];
    for span in &line.spans {
        let style = line.style.patch(span.style);
        let mut buf = String::new();
        for c in span.content.chars() {
            buf.push(c);
            if c == ' ' {
                words.last_mut().unwrap().push((std::mem::take(&mut buf), style));
                words.push(Vec::new());
            }
        }
        if !buf.is_empty() {
            words.last_mut().unwrap().push((buf, style));
        }
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0;
    let indent_w = indent.width();
    let flush = |cur: &mut Vec<Span<'static>>, cur_w: &mut usize, out: &mut Vec<Line<'static>>| {
        let spans = std::mem::take(cur);
        out.push(Line::from(spans));
        *cur_w = indent_w;
        cur.push(Span::raw(indent.to_string()));
    };
    for word in words {
        let ww: usize = word.iter().map(|(s, _)| s.width()).sum();
        let trimmed_w = ww - word.last().map_or(0, |(s, _)| s.len() - s.trim_end_matches(' ').len());
        if cur_w + trimmed_w > width && cur_w > indent_w {
            flush(&mut cur, &mut cur_w, &mut out);
        }
        if cur_w + trimmed_w <= width {
            cur.extend(word.into_iter().map(|(s, st)| Span::styled(s, st)));
            cur_w += ww;
        } else {
            // Word longer than a full row: hard-split by characters.
            for (s, st) in word {
                for c in s.chars() {
                    let cw = c.width().unwrap_or(0);
                    if cur_w + cw > width {
                        flush(&mut cur, &mut cur_w, &mut out);
                    }
                    cur.push(Span::styled(c.to_string(), st));
                    cur_w += cw;
                }
            }
        }
    }
    if cur_w > indent_w || out.is_empty() {
        out.push(Line::from(cur));
    }
    out
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` into unix seconds.
pub(crate) fn parse_ts(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

pub fn ago(ts: &str) -> String {
    let Some(t) = parse_ts(ts) else { return ts.to_string() };
    let s = (now_secs() - t).max(0);
    match s {
        0..60 => "<1m".into(),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        86400..2_592_000 => format!("{}d", s / 86400),
        2_592_000..31_536_000 => format!("{}mo", s / 2_592_000),
        _ => format!("{}y", s / 31_536_000),
    }
}

fn state_icon(state: &str, draft: bool) -> Span<'static> {
    match (state, draft) {
        ("OPEN", true) => Span::styled("◌", Style::new().fg(DIM)),
        ("OPEN", false) => Span::styled("●", Style::new().fg(Color::Green)),
        ("MERGED", _) => Span::styled("◆", Style::new().fg(Color::Magenta)),
        _ => Span::styled("✕", Style::new().fg(Color::Red)),
    }
}

fn state_badge(state: &str, draft: bool) -> Span<'static> {
    let (label, bg) = match (state, draft) {
        ("OPEN", true) => (" DRAFT ", Color::DarkGray),
        ("OPEN", false) => (" OPEN ", Color::Green),
        ("MERGED", _) => (" MERGED ", Color::Magenta),
        _ => (" CLOSED ", Color::Red),
    };
    Span::styled(label, Style::new().fg(Color::Black).bg(bg).bold())
}

fn ci_icon(state: Option<&str>) -> Span<'static> {
    match state {
        Some("SUCCESS") => Span::styled("✓", Style::new().fg(Color::Green)),
        Some("FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE") => {
            Span::styled("✗", Style::new().fg(Color::Red))
        }
        Some("PENDING" | "EXPECTED" | "IN_PROGRESS" | "QUEUED" | "WAITING") => Span::styled("●", Style::new().fg(Color::Yellow)),
        Some("NEUTRAL" | "SKIPPED" | "STALE") => Span::styled("○", Style::new().fg(DIM)),
        _ => Span::styled("·", Style::new().fg(DIM)),
    }
}

fn review_span(decision: Option<&str>) -> Span<'static> {
    match decision {
        Some("APPROVED") => Span::styled("✓ approved", Style::new().fg(Color::Green)),
        Some("CHANGES_REQUESTED") => Span::styled("± changes", Style::new().fg(Color::Red)),
        Some("REVIEW_REQUIRED") => Span::styled("◔ review", Style::new().fg(Color::Yellow)),
        _ => Span::styled("—", Style::new().fg(DIM)),
    }
}

/// The unread gutter mark: a `•` (not colour alone), or a blank keeping the column.
fn unread_dot(unread: bool) -> Span<'static> {
    if unread { Span::styled("•", Style::new().fg(ACCENT).bold()) } else { Span::raw(" ") }
}

/// `⧉2/4`: position / size in a stack; GitHub stacks in the accent, implicit chains gray.
fn chip_span(chip: Option<(usize, usize, bool)>) -> Option<Span<'static>> {
    let (pos, size, native) = chip?;
    Some(Span::styled(format!("⧉{pos}/{size}"), Style::new().fg(if native { ACCENT } else { Color::Gray })))
}

/// A native chip from a row's own fields (a GitHub result that isn't loaded).
fn summary_chip(p: &PrSummary) -> Option<(usize, usize, bool)> {
    Some((p.stack_entry.as_ref()?.position, p.stack.as_ref()?.size, true))
}

/// New comments since you last looked (list counts can't tell bots or yourself apart).
fn new_comments(changes: Option<&Vec<Change>>) -> usize {
    changes.and_then(|c| c.iter().find_map(|c| if let Change::Comments(n) = c { Some(*n) } else { None })).unwrap_or(0)
}

/// The Cmts cell: the count, then `+n` new ones.
fn comment_spans(total: usize, changes: Option<&Vec<Change>>) -> Vec<Span<'static>> {
    let mut out = vec![Span::styled(if total > 0 { total.to_string() } else { String::new() }, Style::new().fg(Color::Gray))];
    let n = new_comments(changes);
    if n > 0 {
        out.push(Span::styled(format!(" +{n}"), Style::new().fg(ACCENT).bold()));
    }
    out
}

// ---------- header / status / footer ----------

fn draw_header(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_widget(Block::new().style(Style::new().bg(Color::Indexed(236))), area);
    let scope = app.scopes[app.scope].name.clone();
    let mut x = area.x;
    let title = " ⎇ GH PR Manager ";
    let mut spans = vec![Span::styled(title, Style::new().fg(Color::Black).bg(ACCENT).bold()), Span::raw(" ")];
    x += title.width() as u16 + 1;
    let scope_label = format!(" ▾ {scope} ");
    let w = scope_label.width() as u16;
    app.hits.buttons.push((Rect::new(x, area.y, w, 1), Action::OpenScopes));
    spans.push(Span::styled(scope_label, Style::new().fg(ACCENT).bold().add_modifier(Modifier::UNDERLINED)));
    spans.push(Span::raw(" "));
    x += w + 1;
    let state_label = format!(" ◐ {} ", app.state_filter.label());
    let w = state_label.width() as u16;
    app.hits.buttons.push((Rect::new(x, area.y, w, 1), Action::CycleState));
    spans.push(Span::styled(state_label, Style::new().fg(Color::Yellow).add_modifier(Modifier::UNDERLINED)));
    spans.push(Span::raw(" "));
    x += w + 1;
    let repo_label = match &app.repo {
        Some(r) => format!(" ◫ {} ", truncate(r, 28)),
        None => " ◫ All repos ▾ ".to_string(),
    };
    let w = repo_label.width() as u16;
    app.hits.buttons.push((Rect::new(x, area.y, w, 1), Action::OpenRepos));
    spans.push(Span::styled(repo_label, Style::new().fg(Color::Blue).bold().add_modifier(Modifier::UNDERLINED)));
    x += w;
    if app.repo.is_some() {
        app.hits.buttons.push((Rect::new(x, area.y, 3, 1), Action::SetRepo(None)));
        spans.push(Span::styled(" ✕ ", Style::new().fg(Color::Black).bg(Color::Blue)));
        x += 3;
    }
    spans.push(Span::raw(" "));
    x += 1;
    let search_label = " ⌕ Search ";
    app.hits.buttons.push((Rect::new(x, area.y, search_label.width() as u16, 1), Action::OpenSearch));
    spans.push(Span::styled(search_label, Style::new().fg(Color::Magenta).bold().add_modifier(Modifier::UNDERLINED)));
    spans.push(Span::raw("  "));
    x += search_label.width() as u16 + 2;

    let shown = app.visible_base().len();
    let count = if app.search.pinned.is_some() {
        format!("{shown} matching of {}", app.prs.len())
    } else if app.total > app.prs.len() {
        format!("{} of {} PRs", app.prs.len(), app.total)
    } else {
        format!("{} PRs", app.prs.len())
    };
    // ` 87 PRs · 4 unread ` (click toggles the unread filter); with it on, ` 4 unread of 87 PRs ✕ `.
    let unread = app.unread_count();
    let unread_style = Style::new().fg(ACCENT).bold().add_modifier(Modifier::UNDERLINED);
    if app.unread_only {
        let label = format!("{unread} unread");
        let w = label.width() as u16;
        app.hits.buttons.push((Rect::new(x, area.y, w, 1), Action::ToggleUnreadOnly));
        spans.push(Span::styled(label, unread_style));
        let of = format!(" of {count} ");
        x += w + of.width() as u16;
        spans.push(Span::styled(of, Style::new().fg(Color::Gray)));
        app.hits.buttons.push((Rect::new(x, area.y, 3, 1), Action::ToggleUnreadOnly));
        spans.push(Span::styled(" ✕ ", Style::new().fg(Color::Black).bg(ACCENT)));
    } else {
        x += count.width() as u16;
        spans.push(Span::styled(count, Style::new().fg(Color::Gray)));
        if unread > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(DIM)));
            let label = format!("{unread} unread");
            app.hits.buttons.push((Rect::new(x + 3, area.y, label.width() as u16, 1), Action::ToggleUnreadOnly));
            spans.push(Span::styled(label, unread_style));
        }
    }
    if app.loading_list || !app.detail_loading.is_empty() || !app.busy.is_empty() {
        spans.push(Span::styled(format!("  {} loading", SPINNER[app.tick % SPINNER.len()]), Style::new().fg(ACCENT)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);

    let mut right = Vec::new();
    let density = format!(" ≡ {} ", app.state.density.label());
    right.push(Span::styled(density.clone(), Style::new().fg(Color::Gray).add_modifier(Modifier::UNDERLINED)));
    right.push(Span::raw(" "));
    if let Some(r) = &app.rate {
        let color = if r.remaining < r.limit / 10 { Color::Red } else { DIM };
        right.push(Span::styled(format!("API {}/{} ", r.remaining, r.limit), Style::new().fg(color)));
    }
    if let Some(v) = &app.viewer {
        right.push(Span::styled(format!("@{} ", v.login), Style::new().fg(Color::Gray)));
    }
    let help = " ? ";
    right.push(Span::styled(help, Style::new().fg(Color::Black).bg(Color::Gray)));
    let rw = right.iter().map(|s| s.width()).sum::<usize>() as u16;
    if rw < area.width / 2 {
        let r = Rect::new(area.right() - rw, area.y, rw, 1);
        // On a narrow screen this covers the end of the left side; so are its click targets.
        app.hits.buttons.retain(|(b, a)| *a != Action::ToggleUnreadOnly || b.right() <= r.x);
        app.hits.buttons.push((Rect::new(area.right() - 3, area.y, 3, 1), Action::Help));
        app.hits.buttons.push((Rect::new(r.x, area.y, density.width() as u16, 1), Action::CycleDensity));
        f.render_widget(Paragraph::new(Line::from(right)), r);
    }
}

fn draw_status(f: &mut Frame, app: &mut App, area: Rect) {
    // The refresh digest beats an older toast, never an error or the log-search prompt.
    let log_search = app.log_view.search.is_some() && app.screen == Screen::Detail && app.tab == Tab::Actions;
    let digest = app.digest.as_ref().filter(|d| !log_search && app.toast.as_ref().is_none_or(|(_, kind, at)| *kind != ToastKind::Error && *at <= d.at));
    let line = if let Some(d) = digest {
        // ↻ 3 PRs changed · #151 CI failed · #437 review requested · +2 more   u unread only
        let mut spans = Vec::new();
        let mut x = area.x;
        // Clicks past the text area belong to the `img:` chip on the right.
        let right = area.right().saturating_sub(format!(" img: {} ", app.images.protocol_label()).width() as u16);
        let mut push = |spans: &mut Vec<Span<'static>>, text: String, style: Style, action: Option<Action>| {
            let w = text.width() as u16;
            if let Some(a) = action
                && x < right
            {
                app.hits.buttons.push((Rect::new(x, area.y, w.min(right - x), 1), a));
            }
            x += w;
            spans.push(Span::styled(text, style));
        };
        let first = d.items.first().map(|(id, _)| Action::JumpTo(id.clone()));
        let head = format!(" ↻ {} PR{} changed{}", d.count, if d.count == 1 { "" } else { "s" }, if d.since_last { " since you last looked" } else { "" });
        push(&mut spans, head, Style::new().fg(ACCENT).bold(), first);
        for (id, text) in &d.items {
            push(&mut spans, " · ".into(), Style::new().fg(DIM), None);
            push(&mut spans, text.clone(), Style::new().add_modifier(Modifier::UNDERLINED), Some(Action::JumpTo(id.clone())));
        }
        if d.more > 0 {
            push(&mut spans, format!(" · +{} more", d.more), Style::new().fg(DIM), None);
        }
        push(&mut spans, "   u unread only · click to jump".into(), Style::new().fg(DIM), None);
        Line::from(spans)
    } else if let Some((msg, kind, _)) = &app.toast {
        let (icon, color) = match kind {
            ToastKind::Info => ("ℹ", ACCENT),
            ToastKind::Success => ("✓", Color::Green),
            ToastKind::Error => ("✗", Color::Red),
        };
        Line::from(vec![Span::styled(format!(" {icon} "), Style::new().fg(color).bold()), Span::styled(msg.clone(), Style::new().fg(color))])
    } else if let Some(s) = app.log_view.search.as_ref().filter(|_| app.screen == Screen::Detail && app.tab == Tab::Actions) {
        let mut spans = vec![Span::styled(" ^F ", Style::new().fg(ACCENT).bold()), Span::raw(s.query.clone())];
        if s.input {
            spans.push(Span::styled("▏", Style::new().fg(ACCENT)));
        }
        let count = match s.hits.len() {
            0 if s.query.is_empty() => String::new(),
            0 => "   no matches".into(),
            n => {
                let cur = app.log_view.cursor.and_then(|c| s.hits.binary_search(&c).ok()).map(|i| format!("{}/", i + 1)).unwrap_or_default();
                format!("   {cur}{n} matches")
            }
        };
        spans.push(Span::styled(count, Style::new().fg(Color::Yellow)));
        let hint = if s.input { "   Enter jump · Esc clear" } else { "   n / N next / previous · Ctrl+F edit · Esc clear" };
        spans.push(Span::styled(hint, Style::new().fg(DIM)));
        Line::from(spans)
    } else if let Some(p) = app.search.pinned.clone() {
        // The pinned query: click it to edit, ✕ to clear.
        let shown = app.visible().len();
        let esc = if app.screen == Screen::List { " · Esc clears" } else { "" };
        let spans = vec![
            Span::styled(" / ", Style::new().fg(ACCENT).bold()),
            Span::styled(truncate(&p, 48), Style::new().bold()),
            Span::styled(format!("   {shown} of {} · click to edit{esc} ", app.prs.len()), Style::new().fg(DIM)),
        ];
        let w = spans.iter().map(|s| s.width()).sum::<usize>() as u16;
        app.hits.buttons.push((Rect::new(area.x, area.y, w.min(area.width), 1), Action::EditSearch));
        app.hits.buttons.push((Rect::new(area.x + w + 1, area.y, 3, 1), Action::ClearSearch));
        let mut spans = spans;
        spans.push(Span::raw(" "));
        spans.push(Span::styled(" ✕ ", Style::new().fg(Color::Black).bg(ACCENT)));
        Line::from(spans)
    } else {
        Line::from(Span::styled(format!(" {}", app.search_query()), Style::new().fg(DIM)))
    };
    // The image protocol in use, on the right; clicking it shows or hides images.
    let chip = format!(" img: {} ", app.images.protocol_label());
    let cw = chip.width() as u16;
    let area = if cw + 20 < area.width {
        let r = Rect::new(area.right() - cw, area.y, cw, 1);
        f.render_widget(Paragraph::new(Span::styled(chip, Style::new().fg(DIM))), r);
        app.hits.buttons.push((r, Action::ToggleImages));
        Rect { width: area.width - cw, ..area }
    } else {
        area
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_footer(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_widget(Block::new().style(Style::new().bg(Color::Indexed(236))), area);
    let key = Style::new().fg(Color::Black).bg(ACCENT);
    let items: Vec<(&str, &str, Action)> = match (&app.modal, app.screen) {
        (Some(Modal::Composer(_)), _) => vec![
            ("^S", "Send", Action::Submit),
            ("@", "Mention", Action::InsertAt),
            ("Esc", "Close (keeps draft)", Action::ModalCancel),
        ],
        (Some(Modal::Image { .. }), _) => vec![("o", "Browser", Action::ModalPick(0)), ("y", "Copy URL", Action::ModalPick(1)), ("Esc", "Close", Action::ModalCancel)],
        (Some(Modal::Search), _) => vec![
            ("↵", "Open", Action::SearchEnter),
            ("^F", "Filter list", Action::PinSearch),
            ("^G", "GitHub", Action::SearchGitHub),
            ("Tab", "Complete", Action::SearchComplete),
            ("Esc", "Close", Action::ModalCancel),
        ],
        (Some(_), _) => vec![("Enter", "Select", Action::ModalPick(usize::MAX)), ("Esc", "Close", Action::ModalCancel)],
        (None, Screen::List) => vec![
            ("↵", "Open", Action::OpenSelected),
            ("s", "Scope", Action::OpenScopes),
            ("S", "Repo", Action::OpenRepos),
            ("f", "State", Action::CycleState),
            ("/", "Search", Action::OpenSearch),
            ("u", "Unread", Action::ToggleUnreadOnly),
            ("c", "Comment", Action::Comment),
            ("m", "Merge", Action::MergeMenu),
            ("o", "Browser", Action::Browser),
            ("r", "Refresh", Action::Refresh),
            ("?", "Help", Action::Help),
            ("q", "Quit", Action::Quit),
        ],
        (None, Screen::Detail) if app.tab == Tab::Stack => vec![
            ("Esc", "Back", Action::StackBack),
            ("Tab", "Switch pane", Action::ToggleFocus),
            ("[ ]", "Tabs", Action::NextTab),
            ("↵", "Show files", Action::StackOpen),
            ("n p", "Move", Action::StackMove(1)),
            ("< >", "Prev / next PR", Action::StackNext),
            ("b", "Update branch", Action::StackUpdateBranch),
            ("L", "Link", Action::StackLink),
            ("M", "Merge up to here", Action::StackMergeUpTo),
            ("O", "Browser", Action::StackOpenBrowser),
            ("r", "Refresh", Action::Refresh),
            ("?", "Help", Action::Help),
        ],
        (None, Screen::Detail) if app.tab == Tab::Actions => vec![
            ("Esc", "Back", Action::ActionsBack),
            ("Tab", "Switch pane", Action::ToggleFocus),
            ("[ ]", "Tabs", Action::NextTab),
            ("/", "Search", Action::OpenSearch),
            ("↵", "Fold / show", Action::TreeToggle),
            ("O", "Run in browser", Action::TreeOpenBrowser),
            ("^F", "Search log", Action::LogSearch),
            ("E", "Next error", Action::LogNextError),
            ("z", "Fold", Action::LogFold),
            ("t", "Times", Action::LogTimestamps),
            ("H", "History", Action::ToggleHistory),
            ("r", "Refresh", Action::Refresh),
            ("?", "Help", Action::Help),
        ],
        (None, Screen::Detail) => vec![
            ("Esc", "Back", Action::Back),
            ("Tab", "Switch pane", Action::ToggleFocus),
            ("[ ]", "Tabs", Action::NextTab),
            ("/", "Search", Action::OpenSearch),
            ("u", "Unread", Action::ToggleUnreadOnly),
            ("c", "Comment", Action::Comment),
            ("m", "Merge", Action::MergeMenu),
            ("a", "Approve", Action::Approve),
            ("x", "Close", Action::CloseOrReopen),
            ("␣", "All actions", Action::Menu),
            ("o", "Browser", Action::Browser),
            ("I", "Images", Action::ToggleImages),
            ("r", "Refresh", Action::Refresh),
            ("?", "Help", Action::Help),
        ],
    };
    // In modals the "Select" hint has no target index; drop it from the clickable set.
    let mut items: Vec<_> = items.into_iter().filter(|(_, _, a)| *a != Action::ModalPick(usize::MAX)).collect();
    // The timeline tabs: n / p walk the NEW entries.
    if app.modal.is_none() && app.screen == Screen::Detail && app.tab.has_new() {
        let at = items.iter().position(|(_, _, a)| *a == Action::ToggleUnreadOnly).map_or(items.len(), |i| i + 1);
        items.insert(at, ("n p", "New", Action::NextNew));
    }
    button_row(f, &mut app.hits, area, &items, key);
}

// ---------- list screen ----------

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    let d = app.state.density;
    let block = Block::new()
        .borders(if d.rules() { Borders::TOP } else { Borders::NONE })
        .border_style(Style::new().fg(DIM))
        .padding(Padding::horizontal(d.side_padding()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let vis = app.visible_rows();
    if vis.is_empty() {
        let msg = if let Some(e) = &app.list_error {
            vec![Line::styled(format!("Failed to load PRs: {e}"), Style::new().fg(Color::Red)), Line::raw(""), Line::raw("Press r to retry")]
        } else if app.loading_list {
            vec![Line::styled(format!("{} Loading pull requests…", SPINNER[app.tick % SPINNER.len()]), Style::new().fg(ACCENT))]
        } else if app.unread_only {
            vec![Line::raw("No unread pull requests here."), Line::styled("u or Esc shows them all", Style::new().fg(DIM))]
        } else if let Some(p) = &app.search.pinned {
            vec![Line::raw(format!("No PRs match “{p}”")), Line::styled("Esc clears the search · / searches again (GitHub too)", Style::new().fg(DIM))]
        } else if let Some(r) = &app.repo {
            vec![
                Line::raw(format!("No pull requests for {} in {r}.", app.scopes[app.scope].name)),
                Line::styled("S picks another repository · s → Everyone shows every PR in it · click ✕ to clear", Style::new().fg(DIM)),
            ]
        } else {
            vec![
                Line::raw("No pull requests in this scope."),
                Line::styled("Press s to choose a different scope, or f to change the state filter.", Style::new().fg(DIM)),
            ]
        };
        let h = msg.len() as u16;
        let r = Rect::new(inner.x, inner.y + inner.height.saturating_sub(h) / 2, inner.width, h.min(inner.height));
        f.render_widget(Paragraph::new(msg).centered(), r);
        return;
    }

    let density = app.state.density;
    // Highlight the pinned query's hits, on the rows that can be on screen only.
    let pinned = app.search.pinned_query();
    let page = inner.height as usize;
    let near = app.table_state.offset().min(app.sel).saturating_sub(page)..app.table_state.offset().max(app.sel) + 2 * page;
    // The `#` column grows to hold a stack's tree glyph and `⧉ 2/4` chip.
    let num_w = vis
        .iter()
        .map(|r| {
            let p = &app.prs[r.idx];
            r.glyph.map_or(0, |g| g.width()) + format!("#{}", p.number).width() + chip_span(app.chip(&p.id)).map_or(0, |c| c.width() + 1)
        })
        .max()
        .unwrap_or(0)
        .clamp(6, 18) as u16;
    let rows: Vec<Row> = vis
        .iter()
        .enumerate()
        .map(|(row, rv)| {
            let i = rv.idx;
            let p = &app.prs[i];
            let mk = pinned.as_ref().filter(|_| near.contains(&row)).map(|q| app.marks(q, i, Some(60))).unwrap_or_default();
            let changes = app.unread.get(&p.id);
            let title_style = if p.is_draft { Style::new().fg(Color::Gray) } else { Style::new() };
            let title_style = if changes.is_some() { title_style.bold() } else { title_style };
            let changed = |f: fn(&Change) -> bool| changes.is_some_and(|c| c.iter().any(f));
            let bold_if = |span: Span<'static>, on: bool| if on { span.bold() } else { span };
            let mut title = hl_spans(&p.title, &mk.title, |_| title_style, usize::MAX);
            if body_only(&mk)
                && let Some((text, ranges)) = &mk.snippet
            {
                title.push(Span::styled(" · ", Style::new().fg(DIM)));
                title.extend(hl_spans(text, &ranges_idx(ranges), |_| Style::new().fg(DIM), usize::MAX));
            }
            // Comfortable spends its second row on what changed, in words, then the stack's.
            let mut title_cell = vec![Line::from(title)];
            if density == Density::Comfortable {
                let mut words: Vec<String> = match changes {
                    Some(c) if c.is_empty() => vec![if p.is_read_by_viewer == Some(false) { "unread on GitHub".to_string() } else { "unread".to_string() }],
                    Some(c) => c.iter().map(Change::words).collect(),
                    None => Vec::new(),
                };
                if let Some(n) = app.needs_rebase_of(&p.id) {
                    words.push(format!("needs rebase ({n} behind)"));
                }
                if !words.is_empty() {
                    title_cell.push(Line::styled(words.join(" · "), Style::new().fg(ACCENT)));
                }
            }
            let mut num = Vec::new();
            if let Some(g) = rv.glyph {
                num.push(Span::styled(g, Style::new().fg(DIM)));
            }
            num.extend(number_spans(p.number, &mk.number));
            if let Some(c) = chip_span(app.chip(&p.id)) {
                num.push(Span::raw(" "));
                num.push(c);
            }
            Row::new(vec![
                Cell::from(Line::from(vec![unread_dot(changes.is_some()), state_icon(&p.state, p.is_draft)])),
                Cell::from(Line::from(hl_spans(&p.repository.name_with_owner, &mk.repo, |_| Style::new().fg(Color::Blue), usize::MAX))),
                Cell::from(Line::from(num)),
                Cell::from(ratatui::text::Text::from(title_cell)),
                Cell::from(Line::from(hl_spans(login_of(&p.author), &mk.author, |_| Style::new(), usize::MAX))),
                Cell::from(bold_if(ci_icon(p.ci_state()), changed(|c| matches!(c, Change::Ci { .. })))),
                Cell::from(bold_if(review_span(p.review_decision.as_deref()), changed(|c| matches!(c, Change::Verdict { .. })))),
                Cell::from(Line::from(comment_spans(p.comments.total_count, changes)).right_aligned()),
                Cell::from(Span::styled(ago(&p.updated_at), Style::new().fg(DIM))),
            ])
            // Comfortable rows are two lines tall (the second holds the change words), so
            // `table_row_h` and click hit-testing stay at 2 per PR.
            .height(density.table_row_h() as u16)
        })
        .collect();
    let widths = [
        Constraint::Length(2),
        Constraint::Percentage(20),
        Constraint::Length(num_w),
        Constraint::Fill(1),
        Constraint::Length(14),
        Constraint::Length(2),
        Constraint::Length(10),
        // `20 +12`
        Constraint::Length(7),
        Constraint::Length(7),
    ];
    let header = Row::new(vec![
        Cell::from(""),
        Cell::from("Repository"),
        Cell::from("#"),
        Cell::from("Title"),
        Cell::from("Author"),
        Cell::from("CI"),
        Cell::from("Review"),
        Cell::from(Line::from("Cmts").right_aligned()),
        Cell::from("Updated"),
    ])
        .style(Style::new().fg(Color::Gray).bold())
        .bottom_margin(0);
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(density.column_spacing())
        .row_highlight_style(Style::new().bg(Color::Indexed(238)).bold())
        .highlight_symbol("▌");
    app.table_state.select(Some(app.sel));
    f.render_stateful_widget(table, inner, &mut app.table_state);
    let rows_area = Rect::new(inner.x, inner.y + 1, inner.width, inner.height.saturating_sub(1));
    app.hits.table = Some((rows_area, app.table_state.offset()));
}

// ---------- detail screen ----------

fn pane_block(title: Line<'static>, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(if focused { BorderType::Thick } else { BorderType::Rounded })
        .border_style(Style::new().fg(if focused { ACCENT } else { DIM }))
        .title(title)
}

fn draw_sidebar(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Sidebar;
    let block = pane_block(Line::from(format!(" Pull requests ({}) ", app.visible().len())).bold(), focused);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = inner.width.saturating_sub(2) as usize;
    let pinned = app.search.pinned_query();
    let page = inner.height as usize / app.state.density.sidebar_item_h();
    let near = app.side_state.offset().min(app.sel).saturating_sub(page)..app.side_state.offset().max(app.sel) + 2 * page;
    let items: Vec<ListItem> = app
        .visible_rows()
        .iter()
        .enumerate()
        .map(|(row, rv)| {
            let i = rv.idx;
            let p = &app.prs[i];
            let mk = pinned.as_ref().filter(|_| near.contains(&row)).map(|q| app.marks(q, i, None)).unwrap_or_default();
            let changes = app.unread.get(&p.id);
            let chip = chip_span(app.chip(&p.id));
            let num = format!("{}#{} {}", rv.glyph.unwrap_or(""), p.number, chip.as_ref().map_or(String::new(), |c| format!("{} ", c.content)));
            // Compact has no second line, so the new-comment count follows the title.
            let extra = match (app.state.density, new_comments(changes)) {
                (Density::Compact, n) if n > 0 => format!(" +{n}"),
                _ => String::new(),
            };
            let title_style = if changes.is_some() { Style::new().bold() } else { Style::new() };
            let mut l1 = vec![unread_dot(changes.is_some()), state_icon(&p.state, p.is_draft), Span::raw(" "), ci_icon(p.ci_state()), Span::raw(" ")];
            if let Some(g) = rv.glyph {
                l1.push(Span::styled(g, Style::new().fg(DIM)));
            }
            l1.extend(number_spans(p.number, &mk.number));
            l1.push(Span::raw(" "));
            if let Some(c) = chip {
                l1.push(c);
                l1.push(Span::raw(" "));
            }
            l1.extend(hl_spans(&p.title, &mk.title, |_| title_style, w.saturating_sub(5 + num.width() + extra.width())));
            if !extra.is_empty() {
                l1.push(Span::styled(extra, Style::new().fg(ACCENT).bold()));
            }
            let l1 = Line::from(l1);
            // The dim second line ends with the top two changes.
            let mut meta = format!("     {} · {} · {}", p.repository.name_with_owner, login_of(&p.author), ago(&p.updated_at));
            let mut tail: Vec<(String, Color)> = changes.map(|c| c.iter().take(2).map(|c| (c.label(), ACCENT)).collect()).unwrap_or_default();
            // Then the stack: `needs rebase` (accent) or `2/4 of stack 196`.
            match app.needs_rebase_of(&p.id) {
                Some(_) => tail.push(("needs rebase".into(), ACCENT)),
                None => tail.extend(app.stack_words(&p.id).map(|w| (w, DIM))),
            }
            let l2 = if tail.is_empty() {
                Line::styled(truncate(&meta, w), Style::new().fg(DIM))
            } else {
                let tail_w: usize = tail.iter().map(|(t, _)| t.width() + 3).sum();
                // Keep the changes in view: shorten the repo/author part first.
                meta = truncate(&meta, w.saturating_sub(tail_w).max(w / 3));
                let mut spans = vec![Span::styled(meta.clone(), Style::new().fg(DIM))];
                let mut used = meta.width();
                for (t, color) in tail {
                    let piece = truncate(&format!(" · {t}"), w.saturating_sub(used));
                    used += piece.width();
                    spans.push(Span::styled(piece, Style::new().fg(color)));
                }
                Line::from(spans)
            };
            match app.state.density {
                Density::Comfortable => ListItem::new(vec![l1, l2, Line::raw("")]),
                Density::Cozy => ListItem::new(vec![l1, l2]),
                Density::Compact => ListItem::new(vec![l1]),
            }
        })
        .collect();
    let list = List::new(items)
        .highlight_style(Style::new().bg(if focused { Color::Indexed(238) } else { Color::Indexed(236) }).bold())
        .highlight_symbol(if focused { "▌" } else { " " });
    app.side_state.select(Some(app.sel));
    f.render_stateful_widget(list, inner, &mut app.side_state);
    app.hits.sidebar = Some((inner, app.side_state.offset()));
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Detail;
    let Some(pr) = app.selected_pr().cloned() else {
        let block = pane_block(Line::from(" Details "), focused);
        f.render_widget(Paragraph::new("No pull request selected").block(block).centered(), area);
        return;
    };
    let title = Line::from(vec![Span::raw(" "), Span::styled(format!("{}#{}", pr.repository.name_with_owner, pr.number), Style::new().bold()), Span::raw(" ")]);
    let block = pane_block(title, focused);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let sep_h = if app.state.density.rules() { 1 } else { 0 };
    let [bar, sep, rest] = Layout::vertical([Constraint::Length(1), Constraint::Length(sep_h), Constraint::Min(1)]).areas(inner);

    // Clickable action bar.
    let fetched_at = app.details.get(&pr.id).map(|(d, at)| (d.state == "CLOSED", *at));
    let close_label = if fetched_at.is_some_and(|(closed, _)| closed) { "Reopen" } else { "Close" };
    let btn = Style::new().fg(Color::Black).bg(ACCENT);
    button_row(
        f,
        &mut app.hits,
        bar,
        &[
            ("c", "Comment", Action::Comment),
            ("m", "Merge ▾", Action::MergeMenu),
            ("a", "Approve", Action::Approve),
            ("x", close_label, Action::CloseOrReopen),
            ("o", "Browser", Action::Browser),
            ("␣", "More", Action::Menu),
        ],
        btn,
    );
    f.render_widget(Paragraph::new("─".repeat(sep.width as usize)).style(Style::new().fg(DIM)), sep);

    let text_w = rest.width.saturating_sub(text_pad() + 1) as usize;

    // PR info header + description (cached per PR, width and fetch).
    let mut info: Vec<DetailRow> = Vec::new();
    match fetched_at {
        Some((_, at)) => {
            let ckey = format!("{}:{:?}", pr.id, density());
            let fresh = app.detail_cache.as_ref().is_some_and(|(id, cw, cat, _)| *id == ckey && *cw == text_w && *cat == at);
            if !fresh {
                let mut built = Vec::new();
                info_lines(&app.details[&pr.id].0, text_w, &mut built);
                app.detail_cache = Some((ckey, text_w, at, built));
            }
            info = app.detail_cache.as_ref().map(|c| c.3.clone()).unwrap_or_default();
        }
        None => {
            summary_lines(&pr, text_w, &mut info);
            info.push((Line::raw(""), Vec::new()));
            if let Some(e) = app.detail_errors.get(&pr.id) {
                info.push((Line::styled(format!("Failed to load details: {e}"), Style::new().fg(Color::Red)), Vec::new()));
                info.push((Line::styled("Press r to retry", Style::new().fg(DIM)), Vec::new()));
            } else {
                info.push((Line::styled(format!("{} Loading details…", SPINNER[app.tick % SPINNER.len()]), Style::new().fg(ACCENT)), Vec::new()));
            }
        }
    }
    if let Some(b) = app.details.get(&pr.id).and_then(|(d, _)| state_banner(d, app.visit.as_ref().filter(|v| v.id == pr.id)?.since.as_ref()?.snap.state.as_str(), text_w)) {
        info.insert(0, (b, Vec::new()));
    }
    // The base ← head line (its last row when it wraps), found by its tag rather than the blank
    // line after it (which Compact density leaves out); the stack strip goes right under it.
    let arrow = info.iter().rposition(|(_, links)| links.iter().any(|(a, b, u)| a == b && u == ARROW_LINK));
    let strip = stack_strip(app, &pr.id, text_w);
    let strip_len = strip.len();
    if let Some(i) = arrow {
        info.splice(i + 1..i + 1, strip);
    }
    let collapsed = app.info_collapsed();
    if collapsed {
        // Keep just the title block: title, badges, the base ← head line and the stack strip.
        let end = arrow.map(|i| i + 1 + strip_len).or_else(|| info.iter().position(|(l, _)| l.width() == 0)).unwrap_or(info.len());
        info.truncate(end);
    }
    let cap = if collapsed { info.len() } else { (rest.height as usize * 45 / 100).max(5) };
    let info_h = info.len().min(cap).min(rest.height.saturating_sub(6) as usize) as u16;
    let margin = app.state.density.tab_margin();
    let [info_area, _, tabs_area, _, content] = Layout::vertical([
        Constraint::Length(info_h),
        Constraint::Length(margin),
        Constraint::Length(1),
        Constraint::Length(margin),
        Constraint::Min(1),
    ])
    .areas(rest);

    let (max, links) = render_rows(f, info_area, &info, &mut app.info_scroll);
    app.hits.info = Some(info_area);
    app.hits.info_max_scroll = max;
    app.hits.info_links = links;
    app.hits.detail_text_x = info_area.x + text_pad();

    draw_tab_bar(f, app, tabs_area);

    // Changes: file list on the left, the selected file's diff on the right.
    if app.tab == Tab::Changes && app.details.contains_key(&pr.id) && app.files.contains_key(&pr.id) {
        return draw_changes(f, app, &pr.id, content);
    }
    // Actions: runs and jobs on the left, the selected one on the right.
    if app.tab == Tab::Actions && app.details.contains_key(&pr.id) {
        return draw_actions(f, app, &pr.id, content);
    }
    // Stack: members on the left, the selected one's files (or a file's patch) on the right.
    if app.tab == Tab::Stack && app.details.contains_key(&pr.id) {
        return draw_stack(f, app, &pr.id, content);
    }

    // Active tab content. Inline images are capped at 2/5 of the screen's height, so the
    // cap is part of the cache key like the width.
    let max_rows = app.cfg.image_max_rows.min((f.area().height * 2 / 5).max(6));
    app.images.max_rows = max_rows;
    let detail = app.details.get(&pr.id).map(|(d, at)| (d, *at));
    let (rows, placements) = match detail {
        None => (vec![(Line::styled(format!("{} Loading…", SPINNER[app.tick % SPINNER.len()]), Style::new().fg(ACCENT)), Vec::new())], Vec::new()),
        Some((d, at)) => {
            let stamp = if app.tab == Tab::Changes { app.files.get(&pr.id).map(|(_, t, _)| *t) } else { Some(at) };
            // The markers depend on the visit (what you'd seen when you opened it).
            let serial = app.visit.as_ref().filter(|v| v.id == pr.id).map_or(0, |v| v.serial);
            let key = format!("{}:{text_w}:{max_rows}:{:?}:{:?}:{serial}", pr.id, app.tab, density());
            match stamp {
                Some(stamp) => {
                    if app.tab_cache.get(&key).is_none_or(|(s, _)| *s != stamp) {
                        let mut rows = Vec::new();
                        let mut img = Imgs::new(&app.images);
                        let news = app.visit.as_ref().filter(|v| v.id == pr.id).and_then(|v| Some(News { marks: &v.marks, since: &v.since.as_ref()?.snap }));
                        match app.tab {
                            Tab::Description => description_lines(d, text_w, &mut rows, &mut img),
                            Tab::Conversation => conversation_lines(d, text_w, &mut rows, &mut img, news),
                            Tab::Changes => {}
                            Tab::Commits => commits_lines(d, text_w, &mut rows, news),
                            Tab::Checks => checks_lines(d, text_w, &mut rows),
                            Tab::Actions | Tab::Stack => {}
                            Tab::Reviews => reviews_lines(d, text_w, &mut rows, &mut img, news),
                        }
                        app.tab_cache.insert(key.clone(), (stamp, (rows, img.out)));
                    }
                    app.tab_cache[&key].1.clone()
                }
                // Changes not loaded yet.
                None => {
                    let line = match app.files_errors.get(&pr.id) {
                        Some(e) => Line::styled(format!("Failed to load changes: {e} — press r to retry"), Style::new().fg(Color::Red)),
                        None => Line::styled(format!("{} Loading changes…", SPINNER[app.tick % SPINNER.len()]), Style::new().fg(ACCENT)),
                    };
                    (vec![(line, Vec::new())], Vec::new())
                }
            }
        }
    };
    // An image above the viewport that just loaded pushes the rest down; follow it so the
    // text being read stays put.
    let view = format!("{}:{:?}", pr.id, app.tab);
    if app.last_placements.0 == view {
        app.detail_scroll = images::anchor(&app.last_placements.1, &placements, app.detail_scroll);
    }
    // `NEW` entries, for n / p and a badge click.
    app.hits.new_rows = rows.iter().enumerate().filter(|(_, (_, l))| l.iter().any(|(a, b, u)| a == b && u == NEW_LINK)).map(|(i, _)| i).collect();
    if std::mem::take(&mut app.jump_new)
        && let Some(&r) = app.hits.new_rows.first()
    {
        app.detail_scroll = r.saturating_sub(1);
    }
    let (max, links) = render_rows(f, content, &rows, &mut app.detail_scroll);
    render_images(f, content, &placements, app.detail_scroll, &mut app.images, app.modal.is_some());
    app.images.want(&placements, app.detail_scroll, content.height as usize);
    app.images.pump(&app.client, &app.tx);
    app.last_placements = (view, placements);
    app.hits.detail = Some(content);
    app.hits.detail_max_scroll = max;
    app.hits.detail_height = content.height as usize;
    app.hits.detail_links = links;
}

fn draw_changes(f: &mut Frame, app: &mut App, id: &str, area: Rect) {
    let files = &app.files[id].0;
    if files.is_empty() {
        f.render_widget(Paragraph::new(Line::styled(" No file changes.", Style::new().fg(DIM))), area);
        return;
    }
    app.file_sel = app.file_sel.min(files.len() - 1);
    let list_w = (area.width * 32 / 100).clamp(24.min(area.width), 56);
    let [list_area, gap, diff_area] = Layout::horizontal([Constraint::Length(list_w), Constraint::Length(1), Constraint::Min(10)]).areas(area);
    f.render_widget(Paragraph::new(vec![Line::styled("│", Style::new().fg(DIM)); gap.height as usize]), gap);

    // File list with a summary header.
    let adds: u64 = files.iter().map(|f| f.additions).sum();
    let dels: u64 = files.iter().map(|f| f.deletions).sum();
    let header = Line::from(vec![
        Span::styled(format!(" {} files ", files.len()), Style::new().bold()),
        Span::styled(format!("+{adds}"), Style::new().fg(Color::Green)),
        Span::raw(" "),
        Span::styled(format!("−{dels}"), Style::new().fg(Color::Red)),
    ]);
    f.render_widget(Paragraph::new(header), Rect::new(list_area.x, list_area.y, list_area.width, 1));
    let rows_area = Rect::new(list_area.x, list_area.y + 1, list_area.width, list_area.height.saturating_sub(1));
    let h = rows_area.height as usize;
    // Keep the selected file in view.
    if app.file_sel < app.file_list_scroll {
        app.file_list_scroll = app.file_sel;
    } else if h > 0 && app.file_sel >= app.file_list_scroll + h {
        app.file_list_scroll = app.file_sel + 1 - h;
    }
    app.file_list_scroll = app.file_list_scroll.min(files.len().saturating_sub(h));
    let w = rows_area.width as usize;
    let lines: Vec<Line> = files
        .iter()
        .enumerate()
        .skip(app.file_list_scroll)
        .take(h)
        .map(|(i, file)| {
            let (letter, color) = file_status(&file.status);
            let counts = format!("+{} −{}", file.additions, file.deletions);
            let path_w = w.saturating_sub(counts.width() + 5);
            let (dir, name) = match file.filename.rfind('/') {
                Some(i) => (&file.filename[..=i], &file.filename[i + 1..]),
                None => ("", file.filename.as_str()),
            };
            // Prefer the file name; trim the directory from the left when space is short.
            let name = truncate(name, path_w);
            let dir_room = path_w.saturating_sub(name.width());
            let dir = if dir.width() <= dir_room {
                dir.to_string()
            } else if dir_room > 1 {
                let tail: String = dir.chars().rev().take(dir_room - 1).collect::<Vec<_>>().into_iter().rev().collect();
                format!("…{tail}")
            } else {
                String::new()
            };
            let pad = w.saturating_sub(dir.width() + name.width() + counts.width() + 4);
            let selected = i == app.file_sel;
            let line = Line::from(vec![
                Span::styled(if selected { "▌" } else { " " }, Style::new().fg(ACCENT)),
                Span::styled(format!("{letter} "), Style::new().fg(color).bold()),
                Span::styled(dir, Style::new().fg(DIM)),
                Span::styled(name, if selected { Style::new().bold() } else { Style::new() }),
                Span::raw(" ".repeat(pad)),
                Span::styled(format!("+{}", file.additions), Style::new().fg(Color::Green)),
                Span::raw(" "),
                Span::styled(format!("−{}", file.deletions), Style::new().fg(Color::Red)),
            ]);
            if selected { line.style(Style::new().bg(Color::Indexed(238))) } else { line }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), rows_area);
    app.hits.file_list = Some((rows_area, app.file_list_scroll));

    // Diff of the selected file (cached per file and width).
    let text_w = diff_area.width.saturating_sub(text_pad() + 1) as usize;
    let stamp = app.files[id].1;
    let key = format!("{id}:{text_w}:file:{}:{:?}", app.file_sel, density());
    if app.tab_cache.get(&key).is_none_or(|(s, _)| *s != stamp) {
        let mut rows = Vec::new();
        file_diff_lines(&app.files[id].0[app.file_sel], text_w, &mut rows);
        app.tab_cache.insert(key.clone(), (stamp, (rows, Vec::new())));
    }
    // The first row is the file's name bar: pin it above the scrolling patch.
    let rows = app.tab_cache[&key].1.0.clone();
    let [bar_area, patch_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(diff_area);
    if let Some((bar, _)) = rows.first() {
        f.render_widget(Paragraph::new(bar.clone()), Rect::new(bar_area.x + 1, bar_area.y, bar_area.width.saturating_sub(2), 1));
    }
    let (max, links) = render_rows(f, patch_area, rows.get(1..).unwrap_or_default(), &mut app.detail_scroll);
    app.hits.detail = Some(patch_area);
    app.hits.detail_max_scroll = max;
    app.hits.detail_height = patch_area.height as usize;
    app.hits.detail_links = links;
}

// ---------- stacks ----------

/// `○` open, `◌` draft, `◆` merged, `✕` closed.
fn member_state(m: &Member) -> Span<'static> {
    match (m.state.as_str(), m.is_draft) {
        ("OPEN", true) => Span::styled("◌", Style::new().fg(DIM)),
        ("OPEN", false) => Span::styled("○", Style::new().fg(Color::Green)),
        ("MERGED", _) => Span::styled("◆", Style::new().fg(Color::Magenta)),
        _ => Span::styled("✕", Style::new().fg(Color::Red)),
    }
}

fn member_state_word(m: &Member) -> &'static str {
    match (m.state.as_str(), m.is_draft) {
        ("OPEN", true) => "draft",
        ("OPEN", false) => "open",
        ("MERGED", _) => "merged",
        _ => "closed",
    }
}

/// `✓A` approved, `✗C` changes requested, `—` otherwise (two columns).
fn member_review(m: &Member) -> Span<'static> {
    match m.review.as_deref() {
        Some("APPROVED") => Span::styled("✓A", Style::new().fg(Color::Green)),
        Some("CHANGES_REQUESTED") => Span::styled("✗C", Style::new().fg(Color::Red)),
        _ => Span::styled("— ", Style::new().fg(DIM)),
    }
}

/// The right-edge flags of a member: `!` needs rebase, `✗` conflicts, `⊘` blocked.
fn member_flags(m: &Member, behind: Option<usize>) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    if behind.is_some() {
        out.push(Span::styled("!", Style::new().fg(Color::Yellow).bold()));
    }
    if m.open() && (m.mergeable.as_deref() == Some("CONFLICTING") || m.merge_state.as_deref() == Some("DIRTY")) {
        out.push(Span::styled("✗", Style::new().fg(Color::Red).bold()));
    } else if m.open() && m.merge_state.as_deref() == Some("BLOCKED") {
        out.push(Span::styled("⊘", Style::new().fg(Color::Yellow)));
    }
    out
}

/// A member's merge readiness in words (glyph, text, colour), when GitHub says.
fn member_merge_words(m: &Member) -> Option<(&'static str, &'static str, Color)> {
    if !m.open() {
        return None;
    }
    if m.mergeable.as_deref() == Some("CONFLICTING") || m.merge_state.as_deref() == Some("DIRTY") {
        return Some(("✗", "conflicts", Color::Red));
    }
    match m.merge_state.as_deref() {
        Some("CLEAN" | "HAS_HOOKS") => Some(("✓", "ready to merge", Color::Green)),
        Some("UNSTABLE") => Some(("!", "checks failing", Color::Yellow)),
        Some("BLOCKED") => Some(("⊘", "blocked", Color::Yellow)),
        Some("BEHIND") => Some(("↓", "behind its base", Color::Yellow)),
        Some("DRAFT") => Some(("◌", "draft", DIM)),
        _ if m.mergeable.as_deref() == Some("MERGEABLE") => Some(("✓", "mergeable", Color::Green)),
        _ => None,
    }
}

fn ci_words(state: Option<&str>) -> &'static str {
    match state {
        Some("SUCCESS") => "CI passed",
        Some("FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE") => "CI failed",
        Some("PENDING" | "EXPECTED" | "IN_PROGRESS" | "QUEUED" | "WAITING") => "CI running",
        Some(_) => "CI neutral",
        None => "no CI",
    }
}

/// `Stack      main ← #1 ← #2 ← [#3] ← #4   3/4 · stack 196`, each `#n` opening that PR. Too
/// wide: members far from the open one fold into `…`.
fn stack_strip(app: &App, id: &str, w: usize) -> Vec<DetailRow> {
    let Some(s) = app.selected_stack() else { return Vec::new() };
    let Some(cur) = s.index_of(id) else { return Vec::new() };
    let gray = Style::new().fg(Color::Gray);
    let field = format!("{:<11}", "Stack");
    let trunk = s.trunk.clone();
    // Each member: its spans, width and link.
    let tokens: Vec<(Vec<Span<'static>>, usize, Option<String>)> = s
        .members
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mut spans = vec![Span::styled(" ← ", gray)];
            let label = if i == cur { format!("[#{}]", m.number) } else { format!("#{}", m.number) };
            let style = if i == cur {
                Style::new().fg(ACCENT).bold()
            } else if m.merged() || m.state == "CLOSED" {
                Style::new().fg(DIM)
            } else {
                Style::new().underlined()
            };
            spans.push(Span::styled(label, style));
            if m.merged() {
                spans.push(Span::styled("✓", Style::new().fg(DIM)));
            } else if m.state == "CLOSED" {
                spans.push(Span::styled("✕", Style::new().fg(DIM)));
            }
            spans.extend(member_flags(m, app.needs_rebase_of(&m.id)).into_iter().filter(|f| f.content != "⊘"));
            let width = spans.iter().map(|s| s.width()).sum();
            (spans, width, (i != cur).then(|| format!("#pr:{}", m.id)))
        })
        .collect();
    let (pos, size, _) = app.chip(id).unwrap_or((cur + 1, s.size, s.native()));
    let right = match s.number() {
        Some(n) => format!("   {pos}/{size} · stack {n}"),
        None => format!("   {pos}/{size} · not linked"),
    };
    let fixed = field.width() + trunk.width() + right.width();
    let ell = " ← …".width();
    let fits = |lo: usize, hi: usize| {
        let mid: usize = tokens[lo..=hi].iter().map(|t| t.1).sum();
        fixed + mid + if lo > 0 { ell } else { 0 } + if hi + 1 < tokens.len() { ell } else { 0 } <= w
    };
    let (mut lo, mut hi) = (cur, cur);
    loop {
        let grew_hi = hi + 1 < tokens.len() && fits(lo, hi + 1);
        if grew_hi {
            hi += 1;
        }
        let grew_lo = lo > 0 && fits(lo - 1, hi);
        if grew_lo {
            lo -= 1;
        }
        if !grew_hi && !grew_lo {
            break;
        }
    }
    let mut spans = vec![Span::styled(field, Style::new().fg(DIM)), Span::styled(truncate(&trunk, 24), Style::new().fg(Color::Magenta))];
    let mut x: usize = spans.iter().map(|s| s.width()).sum();
    let mut links = Vec::new();
    if lo > 0 {
        spans.push(Span::styled(" ← …", gray));
        x += ell;
    }
    for (t, width, link) in &tokens[lo..=hi] {
        if let Some(l) = link {
            // The link covers the number, not the arrow before it.
            links.push(((x + 3) as u16, (x + width) as u16, l.clone()));
        }
        spans.extend(t.iter().cloned());
        x += width;
    }
    if hi + 1 < tokens.len() {
        spans.push(Span::styled(" ← …", gray));
    }
    spans.push(Span::styled(right, Style::new().fg(DIM)));
    vec![(Line::from(spans), links)]
}

fn draw_stack(f: &mut Frame, app: &mut App, id: &str, area: Rect) {
    let Some(s) = app.selected_stack().cloned() else {
        let rows: Vec<DetailRow> = vec![
            (Line::raw("Not in a stack."), Vec::new()),
            (Line::styled("A stack is a GitHub stacked PR, or a chain of loaded PRs where each targets the one below's branch.", Style::new().fg(DIM)), Vec::new()),
        ];
        let (max, links) = render_rows(f, area, &rows, &mut app.detail_scroll);
        app.hits.detail = Some(area);
        app.hits.detail_max_scroll = max;
        app.hits.detail_height = area.height as usize;
        app.hits.detail_links = links;
        return;
    };
    let list_w = (area.width * 32 / 100).clamp(24.min(area.width), 56);
    let [list_area, rule, pane_area] = Layout::horizontal([Constraint::Length(list_w), Constraint::Length(1), Constraint::Min(10)]).areas(area);
    f.render_widget(Paragraph::new(vec![Line::styled("│", Style::new().fg(DIM)); rule.height as usize]), rule);
    draw_stack_list(f, app, &s, id, list_area);

    let entry = app.stack_entries().get(app.stack_cursor()).copied().flatten();
    let member = entry.map(|i| s.members[i].clone());
    let text_w = pane_area.width.saturating_sub(text_pad() + 1) as usize;
    let whole = App::whole_stack_key(&s);
    // (files, when they were fetched, an error) of the entry under the cursor.
    let (files, stamp, error): (Option<Vec<FileDiff>>, Option<std::time::Instant>, Option<String>) = match &member {
        Some(m) => {
            let got = app.files.get(&m.id).filter(|(_, _, sha)| m.head_oid.is_empty() || *sha == m.head_oid);
            (got.map(|(f, ..)| f.clone()), got.map(|(_, at, _)| *at), app.files_errors.get(&m.id).cloned())
        }
        None => {
            let key = whole.as_ref().map(|w| w.0.clone()).unwrap_or_default();
            let got = app.stack_files.get(&key);
            (got.map(|(f, _)| f.clone()), got.map(|(_, at)| *at), app.stack_files_errors.get(&key).cloned())
        }
    };

    // A file's patch, with a one-line way back.
    if app.stack_open
        && let (Some(files), Some(stamp)) = (&files, stamp)
        && !files.is_empty()
    {
        app.stack_file_sel = app.stack_file_sel.min(files.len() - 1);
        let j = app.stack_file_sel;
        let who = member.as_ref().map_or("Whole stack".to_string(), |m| format!("#{}", m.number));
        let key = format!("stack:{}:{}:{j}:{text_w}:{:?}", s.key(), who, density());
        if app.tab_cache.get(&key).is_none_or(|(st, _)| *st != stamp) {
            let mut rows = Vec::new();
            file_diff_lines(&files[j], text_w, &mut rows);
            app.tab_cache.insert(key.clone(), (stamp, (rows, Vec::new())));
        }
        let rows = app.tab_cache[&key].1.0.clone();
        let [head_area, bar_area, patch_area] = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)]).areas(pane_area);
        let head = Line::from(vec![
            Span::styled(" ‹ back ", Style::new().fg(Color::Black).bg(ACCENT)),
            Span::styled(format!("  {who} · file {} of {} · n p files · Esc back", j + 1, files.len()), Style::new().fg(DIM)),
        ]);
        f.render_widget(Paragraph::new(head), Rect::new(head_area.x + 1, head_area.y, head_area.width.saturating_sub(2), 1));
        app.hits.buttons.push((Rect::new(head_area.x + 1, head_area.y, 8, 1), Action::StackBack));
        if let Some((bar, _)) = rows.first() {
            f.render_widget(Paragraph::new(bar.clone()), Rect::new(bar_area.x + 1, bar_area.y, bar_area.width.saturating_sub(2), 1));
        }
        let (max, links) = render_rows(f, patch_area, rows.get(1..).unwrap_or_default(), &mut app.detail_scroll);
        app.hits.detail = Some(patch_area);
        app.hits.detail_max_scroll = max;
        app.hits.detail_height = patch_area.height as usize;
        app.hits.detail_links = links;
        return;
    }
    app.stack_open = false;

    let mut rows: Vec<DetailRow> = Vec::new();
    let gray = Style::new().fg(Color::Gray);
    let totals = files.as_ref().map(|f| (f.len(), f.iter().map(|x| x.additions).sum::<u64>(), f.iter().map(|x| x.deletions).sum::<u64>()));
    match &member {
        Some(m) => {
            let title = vec![
                Span::styled(format!("#{} ", m.number), Style::new().fg(ACCENT).bold()),
                Span::styled(m.title.clone(), if m.id == id { Style::new().bold() } else { Style::new().bold().underlined() }),
            ];
            push_wrapped(&mut rows, Line::from(title), text_w, "  ", (m.id != id).then(|| format!("#pr:{}", m.id)));
            let mut status = vec![member_state(m), Span::styled(format!(" {}", member_state_word(m)), gray), Span::styled(" · ", Style::new().fg(DIM))];
            status.push(ci_icon(m.ci.as_deref()));
            status.push(Span::styled(format!(" {}", ci_words(m.ci.as_deref())), gray));
            status.push(Span::styled(" · ", Style::new().fg(DIM)));
            status.push(review_span(m.review.as_deref()));
            if let Some((g, words, color)) = member_merge_words(m) {
                status.push(Span::styled(" · ", Style::new().fg(DIM)));
                status.push(Span::styled(format!("{g} {words}"), Style::new().fg(color)));
            }
            push_wrapped(&mut rows, Line::from(status), text_w, "  ", None);
            let mut rel = match s.parent_of(&m.id) {
                Some(p) => vec![Span::styled("parent ", Style::new().fg(DIM)), Span::styled(format!("#{} ", p.number), Style::new().underlined()), Span::styled(m.base.clone(), Style::new().fg(Color::Magenta))],
                None => vec![Span::styled("base ", Style::new().fg(DIM)), Span::styled(m.base.clone(), Style::new().fg(Color::Magenta))],
            };
            // Only a successful compare of this member says it's up to date.
            let links_known = app.links.get(&s.key()).is_some_and(|(l, _)| l.iter().any(|l| l.child == m.head));
            match app.needs_rebase_of(&m.id) {
                Some(n) => rel.push(Span::styled(format!(" · ! needs rebase: {n} commit{} behind", if n == 1 { "" } else { "s" }), Style::new().fg(Color::Yellow).bold())),
                None if links_known && m.open() => rel.push(Span::styled(" · up to date with its base", Style::new().fg(Color::Green))),
                None => {}
            }
            if let Some((n, a, d)) = totals {
                rel.push(Span::styled(format!(" · {n} files "), gray));
                rel.push(Span::styled(format!("+{a}"), Style::new().fg(Color::Green)));
                rel.push(Span::raw(" "));
                rel.push(Span::styled(format!("−{d}"), Style::new().fg(Color::Red)));
            }
            let link = s.parent_of(&m.id).map(|p| format!("#pr:{}", p.id));
            push_wrapped(&mut rows, Line::from(rel), text_w, "  ", link);
            if !m.loaded {
                rows.push((Line::styled("not in the loaded list — < > or a click loads it", Style::new().fg(DIM).italic()), Vec::new()));
            }
        }
        None => {
            let (base, top) = (whole.as_ref().map(|w| w.1.clone()).unwrap_or_default(), s.members.last().map(|m| m.number).unwrap_or_default());
            rows.push((Line::from(vec![Span::styled("Whole stack", Style::new().bold()), Span::styled(format!("  {base}…#{top}"), gray)]), Vec::new()));
            let mut l = vec![Span::styled("every open PR's changes together (compare, GitHub stops at 300 files)", Style::new().fg(DIM))];
            if let Some((n, a, d)) = totals {
                l = vec![
                    Span::styled(format!("{n} files "), gray),
                    Span::styled(format!("+{a}"), Style::new().fg(Color::Green)),
                    Span::raw(" "),
                    Span::styled(format!("−{d}"), Style::new().fg(Color::Red)),
                    Span::styled(" · cumulative, GitHub stops at 300 files", Style::new().fg(DIM)),
                ];
            }
            push_wrapped(&mut rows, Line::from(l), text_w, "  ", None);
        }
    }
    section(&mut rows, "Files", text_w);
    match (&files, &error) {
        (Some(files), _) if files.is_empty() => rows.push((Line::styled("No file changes.", Style::new().fg(DIM).italic()), Vec::new())),
        (Some(files), _) => {
            for (i, file) in files.iter().enumerate() {
                let (letter, color) = file_status(&file.status);
                let counts = format!("+{} −{}", file.additions, file.deletions);
                let name = truncate(&file.filename, text_w.saturating_sub(counts.width() + 4));
                let pad = text_w.saturating_sub(name.width() + counts.width() + 3);
                rows.push((
                    Line::from(vec![
                        Span::styled(format!("{letter} "), Style::new().fg(color).bold()),
                        Span::raw(name),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(format!("+{}", file.additions), Style::new().fg(Color::Green)),
                        Span::raw(" "),
                        Span::styled(format!("−{}", file.deletions), Style::new().fg(Color::Red)),
                    ]),
                    full_row_link(&Some(format!("#stackfile:{i}"))),
                ));
            }
        }
        (None, Some(e)) => rows.push((Line::styled(format!("Couldn't load the files: {e} — press r to retry"), Style::new().fg(Color::Red)), Vec::new())),
        (None, None) => rows.push((Line::styled(format!("{} Loading files…", SPINNER[app.tick % SPINNER.len()]), Style::new().fg(ACCENT)), Vec::new())),
    }
    gap(&mut rows);
    push_wrapped(
        &mut rows,
        Line::styled("Enter or click a file shows its diff · n p move · O browser · b update branch · L link · M merge up to here", Style::new().fg(DIM)),
        text_w,
        "",
        None,
    );
    let (max, links) = render_rows(f, pane_area, &rows, &mut app.detail_scroll);
    app.hits.detail = Some(pane_area);
    app.hits.detail_max_scroll = max;
    app.hits.detail_height = pane_area.height as usize;
    app.hits.detail_links = links;
}

/// The Stack tab's left column: a header, `Whole stack`, members top-down, the trunk.
fn draw_stack_list(f: &mut Frame, app: &mut App, s: &Stack, open_id: &str, area: Rect) {
    let header = match s.number() {
        Some(n) => vec![Span::styled(format!(" Stack {n} "), Style::new().bold()), Span::styled(format!("· {} PRs · base {}", s.size, truncate(&s.trunk, 16)), Style::new().fg(Color::Gray))],
        None => vec![
            Span::styled(" Chain ", Style::new().bold()),
            Span::styled(format!("· {} PRs · {} · ", s.members.len(), truncate(&s.trunk, 12)), Style::new().fg(Color::Gray)),
            Span::styled("not linked", Style::new().fg(DIM)),
        ],
    };
    f.render_widget(Paragraph::new(Line::from(header)), Rect::new(area.x, area.y, area.width, 1));
    let rows_area = Rect::new(area.x, area.y + 1, area.width, area.height.saturating_sub(1));
    let w = rows_area.width as usize;
    let comfy = density() == Density::Comfortable;
    let cursor = app.stack_cursor();
    let entries = app.stack_entries();
    let mut rows: Vec<(Line<'static>, Option<usize>)> = Vec::new();
    for (k, entry) in entries.iter().enumerate() {
        let selected = k == cursor;
        let bar = Span::styled(if selected { "▌" } else { " " }, Style::new().fg(ACCENT));
        let line = match entry {
            None => {
                let base = App::whole_stack_key(s).map(|w| w.1).unwrap_or_default();
                let top = s.members.last().map(|m| m.number).unwrap_or_default();
                let label = "  Whole stack";
                let range = format!("{}…#{top}", truncate(&base, 12));
                let stats = App::whole_stack_key(s)
                    .and_then(|w| app.stack_files.get(&w.0))
                    .map(|(files, _)| format!("+{} −{}", files.iter().map(|f| f.additions).sum::<u64>(), files.iter().map(|f| f.deletions).sum::<u64>()));
                // Whatever fits: range and stats, either one, or nothing.
                let room = w.saturating_sub(1 + label.width() + 2);
                let right = [stats.as_ref().map(|st| format!("{range} {st}")), stats.clone(), Some(range)]
                    .into_iter()
                    .flatten()
                    .find(|r| r.width() <= room)
                    .unwrap_or_default();
                let pad = w.saturating_sub(1 + label.width() + right.width() + 1);
                Line::from(vec![bar, Span::styled(label, if selected { Style::new().bold() } else { Style::new() }), Span::raw(" ".repeat(pad)), Span::styled(right, Style::new().fg(DIM))])
            }
            Some(i) => {
                let m = &s.members[*i];
                if comfy {
                    rows.push((Line::raw(""), Some(k)));
                }
                let num_style = if m.id == open_id { Style::new().fg(ACCENT).bold() } else if m.merged() { Style::new().fg(DIM) } else { Style::new() };
                let mut spans = vec![bar, Span::styled(format!("{:<5}", format!("#{}", m.number)), num_style), member_state(m), Span::raw(" "), ci_icon(m.ci.as_deref()), Span::raw(" "), member_review(m), Span::raw(" ")];
                let flags = member_flags(m, app.needs_rebase_of(&m.id));
                let flags_w: usize = flags.iter().map(|s| s.width()).sum();
                let used: usize = spans.iter().map(|s| s.width()).sum();
                let title_style = if m.merged() || m.state == "CLOSED" { Style::new().fg(DIM) } else if selected { Style::new().bold() } else { Style::new() };
                let title = truncate(&m.title, w.saturating_sub(used + flags_w + 1));
                let pad = w.saturating_sub(used + title.width() + flags_w);
                spans.push(Span::styled(title, title_style));
                spans.push(Span::raw(" ".repeat(pad)));
                spans.extend(flags);
                Line::from(spans)
            }
        };
        let line = if selected { line.style(Style::new().bg(Color::Indexed(238))) } else { line };
        rows.push((line, Some(k)));
        // Comfortable spells the signals out under the row.
        if comfy && let Some(i) = entry {
            let m = &s.members[*i];
            let words = match (app.needs_rebase_of(&m.id), member_merge_words(m)) {
                (Some(n), _) => {
                    let parent = s.parent_of(&m.id).map_or(s.trunk.clone(), |p| format!("#{}", p.number));
                    format!("needs rebase: {n} commit{} behind {parent}", if n == 1 { "" } else { "s" })
                }
                (None, Some((_, words, _))) => words.to_string(),
                (None, None) => member_state_word(m).to_string(),
            };
            rows.push((Line::styled(format!("        {}", truncate(&words, w.saturating_sub(8))), Style::new().fg(DIM)), Some(k)));
        }
    }
    rows.push((Line::styled(format!("   {}", truncate(&s.trunk, w.saturating_sub(4))), Style::new().fg(Color::Magenta)), None));
    let h = rows_area.height as usize;
    // Keep the cursor in view unless the wheel moved the list away from it.
    if app.stack_list_follow
        && let Some(cur) = rows.iter().rposition(|(_, e)| *e == Some(cursor))
    {
        let first = rows.iter().position(|(_, e)| *e == Some(cursor)).unwrap_or(cur);
        // A member taller than the pane is pinned to its first row rather than flipping ends.
        let cur = if cur - first >= h { first } else { cur };
        if first < app.stack_list_scroll {
            app.stack_list_scroll = first;
        } else if h > 0 && cur >= app.stack_list_scroll + h {
            app.stack_list_scroll = cur + 1 - h;
        }
    }
    app.stack_list_scroll = app.stack_list_scroll.min(rows.len().saturating_sub(h));
    let lines: Vec<Line> = rows.iter().skip(app.stack_list_scroll).take(h).map(|(l, _)| l.clone()).collect();
    f.render_widget(Paragraph::new(lines), rows_area);
    app.hits.stack_list = Some((rows_area, app.stack_list_scroll));
    app.hits.stack_list_rows = rows.into_iter().map(|(_, e)| e).collect();
}

// ---------- Actions tab ----------

/// Uppercased REST outcome (a completed run's conclusion, else its status), as `ci_icon` reads it.
pub(crate) fn rest_outcome(status: &str, conclusion: Option<&str>) -> String {
    if status == "completed" { conclusion.unwrap_or("neutral").to_uppercase() } else { status.to_uppercase() }
}

/// Sort rank of an outcome: failing, unfinished, passed, everything else.
pub(crate) fn outcome_rank(o: &str) -> u8 {
    match o {
        "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => 0,
        "PENDING" | "EXPECTED" | "IN_PROGRESS" | "QUEUED" | "WAITING" | "REQUESTED" => 1,
        "SUCCESS" => 2,
        _ => 3,
    }
}

/// `4m39s`, `1h02m`.
pub(crate) fn fmt_duration(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// Seconds from `start` to `end`, or to now while it's still running.
fn elapsed(start: Option<&str>, end: Option<&str>) -> Option<i64> {
    let s = parse_ts(start?)?;
    Some(end.and_then(parse_ts).unwrap_or_else(now_secs) - s)
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn run_outcome(r: &WorkflowRunInfo) -> String {
    rest_outcome(r.status(), r.conclusion.as_deref())
}

fn job_outcome(j: &Job) -> String {
    rest_outcome(&j.status, j.conclusion.as_deref())
}

fn draw_actions(f: &mut Frame, app: &mut App, id: &str, area: Rect) {
    let Some((nwo, sha)) = app.details.get(id).map(|(d, _)| (d.repository.name_with_owner.clone(), d.head_ref_oid.clone())) else { return };
    // Nothing to put in two columns yet: one message across the pane.
    let message: Vec<Line<'static>> = match app.current_runs().map(|e| e.runs.is_empty()) {
        None => match app.runs_errors.get(id) {
            Some(e) => vec![
                Line::styled(format!("Couldn't load workflow runs: {e}"), Style::new().fg(Color::Red)),
                Line::styled("press r to retry", Style::new().fg(DIM)),
            ],
            None => vec![Line::styled(format!("{} Loading workflow runs…", SPINNER[app.tick % SPINNER.len()]), Style::new().fg(ACCENT))],
        },
        Some(true) if app.no_workflows.contains(&nwo) => vec![Line::styled("This repository doesn't use GitHub Actions", Style::new().fg(DIM).italic())],
        Some(true) => vec![
            Line::raw(format!("No workflow runs for {}", short_sha(&sha))),
            Line::styled("H shows runs from earlier pushes", Style::new().fg(DIM)),
        ],
        Some(false) => Vec::new(),
    };
    if !message.is_empty() {
        let rows: Vec<DetailRow> = message.into_iter().map(|l| (l, Vec::new())).collect();
        let (max, links) = render_rows(f, area, &rows, &mut app.detail_scroll);
        app.hits.detail = Some(area);
        app.hits.detail_max_scroll = max;
        app.hits.detail_height = area.height as usize;
        app.hits.detail_links = links;
        return;
    }

    let list_w = (area.width * 32 / 100).clamp(24.min(area.width), 56);
    let [list_area, gap, pane_area] = Layout::horizontal([Constraint::Length(list_w), Constraint::Length(1), Constraint::Min(10)]).areas(area);
    f.render_widget(Paragraph::new(vec![Line::styled("│", Style::new().fg(DIM)); gap.height as usize]), gap);
    draw_run_tree(f, app, list_area);

    // Right pane: the selected run or job, then its log (if shown) in the same scroll.
    let text_w = pane_area.width.saturating_sub(text_pad() + 1) as usize;
    let mut rows = Vec::new();
    let mut log_job = None;
    match app.selected_node() {
        Some(TreeNode::Run(i)) => {
            if let Some(e) = app.current_runs() {
                let run = &e.runs[i];
                let jobs = app.jobs.get(&run.id).map(|(j, _)| j.as_slice());
                run_header_lines(run, jobs, text_w, &mut rows);
                jobs_summary_lines(app, run, jobs, text_w, &mut rows);
            }
        }
        Some(TreeNode::Job(..)) => {
            if let Some(job) = app.selected_job().cloned() {
                log_job = job_lines(app, &job, text_w, &mut rows);
            }
        }
        _ => {}
    }
    render_actions_pane(f, app, pane_area, &rows, log_job);
}

/// The runs/jobs tree with a summary header; records `hits.actions_tree`.
fn draw_run_tree(f: &mut Frame, app: &mut App, area: Rect) {
    let tree = app.tree();
    let Some(e) = app.current_runs() else { return };
    let (mut fail, mut live, mut pass) = (0, 0, 0);
    for r in e.runs.iter().filter(|r| r.head_sha == e.sha) {
        match outcome_rank(&run_outcome(r)) {
            0 => fail += 1,
            1 => live += 1,
            2 => pass += 1,
            _ => {}
        }
    }
    let n = e.runs.iter().filter(|r| r.head_sha == e.sha).count();
    let mut header = vec![Span::styled(format!(" {n} run{} ", if n == 1 { "" } else { "s" }), Style::new().bold())];
    for (count, outcome) in [(fail, "FAILURE"), (live, "IN_PROGRESS"), (pass, "SUCCESS")] {
        if count > 0 {
            header.push(Span::styled(format!("· {count} "), Style::new().fg(Color::Gray)));
            header.push(ci_icon(Some(outcome)));
            header.push(Span::raw(" "));
        }
    }
    if e.history {
        header.push(Span::styled("· history", Style::new().fg(DIM)));
    }
    f.render_widget(Paragraph::new(Line::from(header)), Rect::new(area.x, area.y, area.width, 1));

    let rows_area = Rect::new(area.x, area.y + 1, area.width, area.height.saturating_sub(1));
    let w = rows_area.width as usize;
    let comfy = density() == Density::Comfortable;
    let mut rows: Vec<(Line<'static>, Option<usize>)> = Vec::new();
    for (i, node) in tree.iter().enumerate() {
        let selected = i == app.actions_sel;
        match node {
            TreeNode::Push(sha) => {
                if comfy && !rows.is_empty() {
                    rows.push((Line::raw(""), None));
                }
                let label = format!(" ── earlier push {} ", short_sha(sha));
                let fill = w.saturating_sub(label.width());
                rows.push((Line::styled(format!("{label}{}", "─".repeat(fill)), Style::new().fg(DIM)), None));
            }
            TreeNode::Run(r) => {
                let run = &e.runs[*r];
                if comfy && i > 0 && !matches!(tree[i - 1], TreeNode::Push(_)) {
                    rows.push((Line::raw(""), None));
                }
                let fold = if app.actions_expanded.contains(&run.id) { "▾ " } else { "▸ " };
                let (right, color) = if run.status() == "completed" {
                    (format!("#{} {}", run.run_number, elapsed(run.run_started_at.as_deref(), Some(&run.updated_at)).map(fmt_duration).unwrap_or_default()), DIM)
                } else {
                    let status = if run.status() == "in_progress" { "running" } else { run.status() };
                    (format!("#{} {status}", run.run_number), Color::Yellow)
                };
                let lead = vec![Span::styled(fold, Style::new().fg(DIM)), ci_icon(Some(&run_outcome(run))), Span::raw(" ")];
                rows.push((tree_row(selected, lead, run.name(), Style::new().bold(), right, color, w), Some(i)));
            }
            TreeNode::Job(run_id, j) => {
                let Some(job) = app.jobs.get(run_id).and_then(|(jobs, _)| jobs.get(*j)) else { continue };
                let dur = elapsed(job.started_at.as_deref(), job.completed_at.as_deref());
                let (right, color) = match job.status.as_str() {
                    "completed" => (dur.map(fmt_duration).unwrap_or_default(), DIM),
                    "in_progress" => (dur.map(fmt_duration).unwrap_or_default(), Color::Yellow),
                    s => (s.to_string(), Color::Yellow),
                };
                let lead = vec![Span::raw("    "), ci_icon(Some(&job_outcome(job))), Span::raw(" ")];
                rows.push((tree_row(selected, lead, &job.name, Style::new(), right, color, w), Some(i)));
            }
        }
    }
    let h = rows_area.height as usize;
    // Keep the cursor in view unless the wheel moved the tree away from it.
    if app.actions_tree_follow
        && let Some(cur) = rows.iter().position(|(_, n)| *n == Some(app.actions_sel))
    {
        if cur < app.actions_tree_scroll {
            app.actions_tree_scroll = cur;
        } else if h > 0 && cur >= app.actions_tree_scroll + h {
            app.actions_tree_scroll = cur + 1 - h;
        }
    }
    app.actions_tree_scroll = app.actions_tree_scroll.min(rows.len().saturating_sub(h));
    let lines: Vec<Line> = rows.iter().skip(app.actions_tree_scroll).take(h).map(|(l, _)| l.clone()).collect();
    f.render_widget(Paragraph::new(lines), rows_area);
    app.hits.actions_tree = Some((rows_area, app.actions_tree_scroll));
    app.hits.actions_tree_rows = rows.into_iter().map(|(_, n)| n).collect();
}

/// One tree row: cursor bar, lead spans, a name truncated to fit, and a right-aligned note.
fn tree_row(selected: bool, lead: Vec<Span<'static>>, name: &str, name_style: Style, right: String, right_color: Color, w: usize) -> Line<'static> {
    let lead_w = 1 + lead.iter().map(|s| s.width()).sum::<usize>();
    let name = truncate(name, w.saturating_sub(lead_w + right.width() + 2));
    let pad = w.saturating_sub(lead_w + name.width() + right.width() + 1);
    let mut spans = vec![Span::styled(if selected { "▌" } else { " " }, Style::new().fg(ACCENT))];
    spans.extend(lead);
    spans.push(Span::styled(name, if selected { name_style.bold() } else { name_style }));
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right, Style::new().fg(right_color)));
    let line = Line::from(spans);
    if selected { line.style(Style::new().bg(Color::Indexed(238))) } else { line }
}

/// A run's title row (click opens it) and summary.
fn run_header_lines(run: &WorkflowRunInfo, jobs: Option<&[Job]>, w: usize, rows: &mut Vec<DetailRow>) {
    let link = Some(run.html_url.clone());
    let mut meta = vec![format!("#{}", run.run_number)];
    if run.run_attempt > 1 {
        meta.push(format!("attempt {}", run.run_attempt));
    }
    meta.push(run.event.clone());
    meta.push(short_sha(&run.head_sha));
    if let Some(d) = elapsed(run.run_started_at.as_deref(), (run.status() == "completed").then_some(run.updated_at.as_str())) {
        meta.push(fmt_duration(d));
    }
    if let Some(a) = run.triggering_actor.as_ref().or(run.actor.as_ref()) {
        meta.push(a.login.clone());
    }
    push_wrapped(
        rows,
        Line::from(vec![
            ci_icon(Some(&run_outcome(run))),
            Span::raw(" "),
            Span::styled(run.name().to_string(), Style::new().bold().underlined()),
            Span::styled(format!("  {}", meta.join(" · ")), Style::new().fg(Color::Gray)),
        ]),
        w,
        "  ",
        link,
    );
    let summary = match jobs {
        None => "loading jobs…".to_string(),
        Some(jobs) => {
            let failed = jobs.iter().filter(|j| outcome_rank(&job_outcome(j)) == 0).count();
            let done = jobs.iter().filter(|j| j.status == "completed").count();
            if failed > 0 {
                format!("failed {failed} of {} jobs", jobs.len())
            } else if done < jobs.len() {
                format!("{done} of {} jobs done", jobs.len())
            } else {
                format!("{} job{} done", jobs.len(), if jobs.len() == 1 { "" } else { "s" })
            }
        }
    };
    rows.push((
        Line::from(vec![
            Span::styled(format!("{summary} · opened {} ago", ago(&run.created_at)), Style::new().fg(Color::Gray)),
            Span::styled("   O opens it in the browser", Style::new().fg(DIM)),
        ]),
        Vec::new(),
    ));
    if !run.display_title.is_empty() {
        rows.push((Line::styled(truncate(&run.display_title, w), Style::new().fg(DIM)), Vec::new()));
    }
}

/// The run's jobs, each clickable to show it, with the step a failed one stopped at.
fn jobs_summary_lines(app: &App, run: &WorkflowRunInfo, jobs: Option<&[Job]>, w: usize, rows: &mut Vec<DetailRow>) {
    section(rows, "Jobs", w);
    let Some(jobs) = jobs else {
        let line = match app.jobs_errors.get(&run.id) {
            Some(e) => Line::styled(format!("Couldn't load jobs: {e} — press r to retry"), Style::new().fg(Color::Red)),
            None => Line::styled(format!("{} Loading jobs…", SPINNER[app.tick % SPINNER.len()]), Style::new().fg(ACCENT)),
        };
        rows.push((line, Vec::new()));
        return;
    };
    if jobs.is_empty() {
        rows.push((Line::styled("No jobs ran in this attempt.", Style::new().fg(DIM).italic()), Vec::new()));
    }
    let name_w = jobs.iter().map(|j| j.name.width()).max().unwrap_or(0).min(w / 2);
    for job in jobs {
        let dur = elapsed(job.started_at.as_deref(), job.completed_at.as_deref()).map(fmt_duration).unwrap_or_else(|| job.status.replace('_', " "));
        let mut spans = vec![
            Span::raw(" "),
            ci_icon(Some(&job_outcome(job))),
            Span::raw(" "),
            Span::styled(format!("{:<name_w$}", truncate(&job.name, name_w)), Style::new().underlined()),
            Span::styled(format!("  {dur:>6}"), Style::new().fg(DIM)),
        ];
        if let Some(s) = job.steps.iter().find(|s| outcome_rank(&rest_outcome(&s.status, s.conclusion.as_deref())) == 0) {
            let used = 4 + name_w + 8;
            spans.push(Span::styled(truncate(&format!("   failed at step {} “{}”", s.number, s.name), w.saturating_sub(used)), Style::new().fg(Color::Red)));
        } else if let Some(s) = job.steps.iter().find(|s| s.status == "in_progress") {
            let used = 4 + name_w + 8;
            spans.push(Span::styled(truncate(&format!("   running step {} “{}”", s.number, s.name), w.saturating_sub(used)), Style::new().fg(Color::Yellow)));
        }
        rows.push((Line::from(spans), full_row_link(&Some(format!("#job:{}", job.id)))));
    }
    gap(rows);
    rows.push((Line::styled("Enter folds a run · Enter or click on a job shows its log", Style::new().fg(DIM)), Vec::new()));
}

/// A job's header, steps and annotations, then the log section. Returns the job id when
/// its log is loaded and should follow these rows.
fn job_lines(app: &App, job: &Job, w: usize, rows: &mut Vec<DetailRow>) -> Option<u64> {
    let log = app.logs.get(&job.id).map(|(l, _)| l);
    let mut meta = Vec::new();
    if !job.labels.is_empty() {
        meta.push(job.labels.join(", "));
    }
    if let Some(d) = elapsed(job.started_at.as_deref(), job.completed_at.as_deref()) {
        meta.push(fmt_duration(d));
    }
    if job.run_attempt > 1 {
        meta.push(format!("attempt {}", job.run_attempt));
    }
    push_wrapped(
        rows,
        Line::from(vec![
            ci_icon(Some(&job_outcome(job))),
            Span::raw(" "),
            Span::styled(job.name.clone(), Style::new().bold().underlined()),
            Span::styled(format!("  {}", meta.join(" · ")), Style::new().fg(Color::Gray)),
            Span::styled("   O browser", Style::new().fg(DIM)),
        ]),
        w,
        "  ",
        job.html_url.clone(),
    );
    steps_lines(job, log, w, rows);
    if let Some(anns) = app.annotations.get(&job.id).filter(|a| !a.is_empty()) {
        annotation_lines(job, anns, log, w, rows);
    }

    let wanted = app.log_wanted.contains(&job.id);
    let state = |rows: &mut Vec<DetailRow>, line: Line<'static>, link: Option<String>| {
        section(rows, "Log", w);
        rows.push((line, full_row_link(&link)));
    };
    if let (true, Some(log)) = (wanted, log) {
        let errors = log.errors.len();
        let title = format!("Log · {} lines · {errors} error{}", log.shown_len(), if errors == 1 { "" } else { "s" });
        gap(rows);
        let label = format!("── {title} ");
        let hint = " ^F search · E error · z fold · t times ";
        let hint = if label.width() + hint.width() + 2 <= w { hint } else { "" };
        let fill = w.saturating_sub(label.width() + hint.width());
        rows.push((
            Line::from(vec![Span::styled(label, Style::new().fg(ACCENT).bold()), Span::styled("─".repeat(fill), Style::new().fg(DIM)), Span::styled(hint, Style::new().fg(DIM))]),
            Vec::new(),
        ));
        if log.dropped_lines > 0 {
            rows.push((
                Line::styled(format!("… {} earlier lines dropped — O opens the full log", log.dropped_lines), Style::new().fg(DIM).italic()),
                full_row_link(&job.html_url),
            ));
        }
        if log.lines.is_empty() {
            rows.push((Line::styled("The log is empty.", Style::new().fg(DIM).italic()), Vec::new()));
        }
        return Some(job.id);
    }
    if !wanted {
        state(rows, Line::styled("Enter (or click the job) shows its log", Style::new().fg(DIM)), Some(format!("#job:{}", job.id)));
    } else if job.status != "completed" {
        state(rows, Line::styled(format!("Log appears when the job finishes (polling every {} s)", crate::app::POLL_SECS), Style::new().fg(Color::Yellow)), None);
    } else if let Some(e) = app.log_errors.get(&job.id) {
        state(rows, Line::styled(format!("Couldn't load the log: {e} — press r to retry"), Style::new().fg(Color::Red)), None);
    } else if app.logs_pending.contains_key(&job.id) && !app.logs_loading.contains(&job.id) {
        state(rows, Line::styled("The log isn't available yet; trying again shortly", Style::new().fg(Color::Yellow)), None);
    } else {
        state(rows, Line::styled(format!("{} Fetching log…", SPINNER[app.tick % SPINNER.len()]), Style::new().fg(ACCENT)), None);
    }
    None
}

/// Steps with durations; clicking one jumps to its part of the log (or loads the log).
fn steps_lines(job: &Job, log: Option<&ParsedLog>, w: usize, rows: &mut Vec<DetailRow>) {
    section(rows, "Steps", w);
    if job.steps.is_empty() {
        rows.push((Line::styled("No steps reported yet.", Style::new().fg(DIM).italic()), Vec::new()));
    }
    let num_w = job.steps.iter().map(|s| s.number.to_string().len()).max().unwrap_or(1);
    for (i, s) in job.steps.iter().enumerate() {
        let outcome = rest_outcome(&s.status, s.conclusion.as_deref());
        let right = match (s.conclusion.as_deref(), elapsed(s.started_at.as_deref(), s.completed_at.as_deref())) {
            (Some("skipped"), _) => "skipped".to_string(),
            (_, Some(d)) if s.status != "queued" && s.status != "pending" => fmt_duration(d),
            _ => s.status.replace('_', " "),
        };
        let name = truncate(&s.name, w.saturating_sub(num_w + right.width() + 7));
        let pad = w.saturating_sub(num_w + name.width() + right.width() + 5);
        let failed = outcome_rank(&outcome) == 0;
        let link = match log.and_then(|l| l.step_first.get(i).copied().flatten()) {
            Some(line) => Some(format!("#line:{line}")),
            None if log.is_none() && job.status == "completed" => Some(format!("#job:{}", job.id)),
            None => None,
        };
        rows.push((
            Line::from(vec![
                Span::raw(" "),
                ci_icon(Some(&outcome)),
                Span::styled(format!(" {:>num_w$} ", s.number), Style::new().fg(DIM)),
                Span::styled(name, if failed { Style::new().fg(Color::Red).bold() } else { Style::new() }),
                Span::raw(" ".repeat(pad)),
                Span::styled(right, Style::new().fg(if s.status == "in_progress" { Color::Yellow } else { DIM })),
            ]),
            full_row_link(&link),
        ));
    }
}

/// The log line an annotation points at: the error or warning row carrying its message.
fn annotation_line(log: &ParsedLog, a: &Annotation) -> Option<usize> {
    let msg = a.message.lines().next().unwrap_or("").trim();
    let targets = if a.annotation_level == "warning" { &log.warnings } else { &log.errors };
    targets.iter().copied().find(|&i| !msg.is_empty() && log.lines[i].plain.contains(msg)).or_else(|| targets.first().copied())
}

fn annotation_lines(job: &Job, anns: &[Annotation], log: Option<&ParsedLog>, w: usize, rows: &mut Vec<DetailRow>) {
    section(rows, &format!("Annotations ({})", anns.len()), w);
    for a in anns {
        let (outcome, color) = match a.annotation_level.as_str() {
            "failure" => ("FAILURE", Color::Red),
            "warning" => ("PENDING", Color::Yellow),
            _ => ("NEUTRAL", ACCENT),
        };
        let icon = if a.annotation_level == "warning" { Span::styled("!", Style::new().fg(Color::Yellow)) } else { ci_icon(Some(outcome)) };
        let loc = match a.start_line {
            Some(l) if l > 0 => format!("{}:{l}", a.path),
            _ => a.path.clone(),
        };
        let msg = match &a.title {
            Some(t) if !t.is_empty() && !a.message.starts_with(t.as_str()) => format!("{t}: {}", a.message),
            _ => a.message.clone(),
        };
        let link = match log {
            Some(l) => annotation_line(l, a).map(|i| format!("#line:{i}")),
            None if job.status == "completed" => Some(format!("#job:{}", job.id)),
            None => None,
        };
        let msg = truncate(msg.lines().next().unwrap_or(""), w.saturating_sub(loc.width() + 6));
        rows.push((
            Line::from(vec![
                Span::raw(" "),
                icon,
                Span::raw(" "),
                Span::styled(loc, Style::new().fg(Color::Magenta)),
                Span::raw("  "),
                Span::styled(msg, Style::new().fg(color)),
            ]),
            full_row_link(&link),
        ));
    }
}

/// `HH:MM:SS` (UTC) of unix seconds.
fn clock(t: i64) -> String {
    let s = t.rem_euclid(86400);
    format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

/// Cut styled spans to `max` columns, ending with `…` when something was cut.
fn clip_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|s| s.width()).sum();
    if total <= max {
        return spans;
    }
    let mut out = Vec::new();
    let mut w = 0;
    for s in spans {
        let sw = s.width();
        if w + sw < max {
            w += sw;
            out.push(s);
            continue;
        }
        let mut text = String::new();
        for c in s.content.chars() {
            let cw = c.width().unwrap_or(0);
            if w + cw + 1 > max {
                break;
            }
            text.push(c);
            w += cw;
        }
        out.push(Span::styled(text, s.style));
        break;
    }
    out.push(Span::styled("…", Style::new().fg(DIM)));
    out
}

/// One log line as a row: fold headers get ▸/▾, a line count and the step they start.
fn log_row(log: &ParsedLog, i: usize, view: &LogView, steps: &[Step], w: usize) -> DetailRow {
    let line = &log.lines[i];
    let mut spans = Vec::new();
    let mut used = 0;
    if view.timestamps {
        let ts = line.ts.map(clock).unwrap_or_else(|| " ".repeat(8));
        spans.push(Span::styled(format!("{ts}  "), Style::new().fg(DIM)));
        used += 10;
    }
    let mut links = Vec::new();
    if let (Kind::Group, Some(g)) = (line.kind, line.group) {
        let group = &log.groups[g];
        let folded = view.folds.contains(&g);
        let n = log.lines[group.start + 1..=group.end].iter().filter(|l| l.kind != Kind::EndGroup).count();
        // Name the step on the group that starts it.
        let starts_step = line.step.filter(|_| g == 0 || log.lines[log.groups[g - 1].start].step != line.step);
        let mut suffix = match starts_step.and_then(|s| steps.get(s)) {
            Some(s) => {
                let d = elapsed(s.started_at.as_deref(), s.completed_at.as_deref()).map(fmt_duration).unwrap_or_default();
                format!("{} · {d}", truncate(&s.name, 32))
            }
            None => String::new(),
        };
        if folded && n > 0 {
            suffix = format!("{suffix}  ({n} line{})", if n == 1 { "" } else { "s" }).trim_start().to_string();
        }
        let icon_color = if group.has_error { Color::Red } else { ACCENT };
        spans.push(Span::styled(if folded { "▸ " } else { "▾ " }, Style::new().fg(icon_color).bold()));
        let title = truncate(&group.title, w.saturating_sub(used + suffix.width() + 4));
        let pad = w.saturating_sub(used + 2 + title.width() + suffix.width() + 1);
        spans.push(Span::styled(title, Style::new().bold()));
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(suffix, Style::new().fg(DIM)));
        links.push((0, u16::MAX, format!("#fold:{g}")));
    } else {
        if line.group.is_some() {
            spans.push(Span::raw("  "));
            used += 2;
        }
        let (base, label) = match line.kind {
            Kind::Error => (Style::new().fg(Color::Red), "Error: "),
            Kind::Warning => (Style::new().fg(Color::Yellow), "Warning: "),
            Kind::Notice => (Style::new().fg(Color::Cyan), "Notice: "),
            Kind::Debug => (Style::new().fg(DIM), ""),
            Kind::Command | Kind::Section => (Style::new().bold(), ""),
            _ => (Style::new(), ""),
        };
        let mut content = Vec::new();
        if !label.is_empty() {
            content.push(Span::styled(label, base.bold()));
        }
        content.extend(ansi_spans(&line.text, base));
        spans.extend(clip_spans(content, w.saturating_sub(used)));
    }
    let hit = view.search.as_ref().is_some_and(|s| s.hits.binary_search(&i).is_ok());
    let row = Line::from(spans);
    let row = if view.cursor == Some(i) {
        row.style(Style::new().bg(Color::Indexed(238)))
    } else if hit {
        row.style(Style::new().bg(Color::Indexed(58)))
    } else {
        row
    };
    (row, links)
}

/// Render `pre` rows followed by the shown log's visible lines as one scrolling region.
/// Only the rows on screen are built, so a 200k-line log costs O(height) per frame.
fn render_actions_pane(f: &mut Frame, app: &mut App, area: Rect, pre: &[DetailRow], log_job: Option<u64>) {
    let height = area.height as usize;
    if let Some(job) = log_job {
        app.sync_log_view(job);
    }
    let log_len = if log_job.is_some() { app.log_view.visible.len() } else { 0 };
    let total = pre.len() + log_len;
    if let Some(line) = app.log_view.scroll_to.take()
        && log_job.is_some()
    {
        let p = app.log_view.visible.partition_point(|&v| v < line);
        app.detail_scroll = (pre.len() + p).saturating_sub(height / 3);
    }
    let max_scroll = total.saturating_sub(height);
    app.detail_scroll = app.detail_scroll.min(max_scroll);
    let scroll = app.detail_scroll;
    let pad = text_pad();
    let text_area = Rect::new(area.x + pad, area.y, area.width.saturating_sub(pad + 1), area.height);
    let w = text_area.width as usize;
    let steps: Vec<Step> = app.selected_job().map(|j| j.steps.clone()).unwrap_or_default();
    let log = log_job.and_then(|j| app.logs.get(&j)).map(|(l, _)| l);
    let mut lines = Vec::new();
    let mut links = Vec::new();
    app.hits.log_top_line = None;
    for row in scroll..(scroll + height).min(total) {
        if let Some((l, k)) = pre.get(row) {
            lines.push(l.clone());
            links.push(k.clone());
        } else if let Some(log) = log {
            let i = app.log_view.visible[row - pre.len()];
            app.hits.log_top_line.get_or_insert(i);
            let (l, k) = log_row(log, i, &app.log_view, &steps, w);
            lines.push(l);
            links.push(k);
        }
    }
    f.render_widget(Paragraph::new(lines), text_area);
    if max_scroll > 0 {
        let mut sb = ScrollbarState::new(max_scroll).position(scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None).thumb_style(Style::new().fg(ACCENT)),
            area,
            &mut sb,
        );
    }
    app.hits.detail = Some(area);
    app.hits.detail_max_scroll = max_scroll;
    app.hits.detail_height = height;
    app.hits.detail_links = links;
    app.hits.detail_links_from = scroll;
}

/// Render a scrollable region of rows with a scrollbar; returns (max scroll, per-row links).
fn render_rows(f: &mut Frame, area: Rect, rows: &[DetailRow], scroll: &mut usize) -> (usize, Vec<Vec<(u16, u16, String)>>) {
    let height = area.height as usize;
    let max_scroll = rows.len().saturating_sub(height);
    *scroll = (*scroll).min(max_scroll);
    let visible: Vec<Line> = rows.iter().skip(*scroll).take(height).map(|(l, _)| l.clone()).collect();
    let pad = text_pad();
    let text_area = Rect::new(area.x + pad, area.y, area.width.saturating_sub(pad + 1), area.height);
    f.render_widget(Paragraph::new(visible), text_area);
    if max_scroll > 0 {
        let mut sb = ScrollbarState::new(max_scroll).position(*scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None).thumb_style(Style::new().fg(ACCENT)),
            area,
            &mut sb,
        );
    }
    (max_scroll, rows.iter().map(|(_, l)| l.clone()).collect())
}

/// Draw the ready images over the rows reserved for them in a `render_rows` region. Partly
/// scrolled-out images are cropped to the rows on screen. An image still being encoded for
/// this size, or any image while a modal is open (its cells can't be dimmed), is a `▒` box.
fn render_images(f: &mut Frame, area: Rect, placements: &[ImagePlacement], scroll: usize, store: &mut ImageStore, modal_open: bool) {
    let pad = text_pad();
    let text_area = Rect::new(area.x + pad, area.y, area.width.saturating_sub(pad + 1), area.height);
    let height = text_area.height as usize;
    for p in placements {
        if p.row + p.rows as usize <= scroll || p.row >= scroll + height || !matches!(store.look(&p.key), Look::Ready(_)) {
            continue;
        }
        let y = p.row as isize - scroll as isize;
        let proto = if modal_open { None } else { store.proto(&p.key, Size::new(p.cols, p.rows)) };
        match proto {
            Some(proto) => f.render_widget(SlicedImage::new(proto, SignedPosition { x: p.x as i16, y: y as i16 }), text_area),
            None => image_box(f.buffer_mut(), text_area, Rect::new(p.x, 0, p.cols, p.rows), y, &p.alt),
        }
    }
}

/// A dim `▒` box with the alt text in the middle, standing in for an image. `r` is relative to
/// `area` with its top at row `y` (negative when scrolled partly out).
fn image_box(buf: &mut Buffer, area: Rect, r: Rect, y: isize, alt: &str) {
    let w = r.width.min(area.width.saturating_sub(r.x));
    let label = truncate(alt, (w as usize).saturating_sub(2));
    let mid = r.height as isize / 2;
    for row in 0..r.height as isize {
        let sy = y + row;
        if sy < 0 || sy >= area.height as isize {
            continue;
        }
        let line = if row == mid {
            let left = (w as usize).saturating_sub(label.width()) / 2;
            let right = (w as usize).saturating_sub(left + label.width());
            Line::from(vec![Span::styled("▒".repeat(left), Style::new().fg(DIM)), Span::styled(label.clone(), Style::new().fg(Color::Gray)), Span::styled("▒".repeat(right), Style::new().fg(DIM))])
        } else {
            Line::styled("▒".repeat(w as usize), Style::new().fg(DIM))
        };
        buf.set_line(area.x + r.x, area.y + sy as u16, &line, w);
    }
}

fn draw_tab_bar(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_widget(Block::new().style(Style::new().bg(Color::Indexed(236))), area);
    let d = app.selected_detail();
    let chip = app.selected_pr().and_then(|p| app.chip(&p.id));
    let count = |t: Tab| -> String {
        if t == Tab::Stack {
            return chip.map_or(" –".into(), |(pos, size, _)| format!(" {pos}/{size}"));
        }
        let Some(d) = d else { return String::new() };
        match t {
            Tab::Description => String::new(),
            Tab::Conversation => format!(" {}", d.comments.iter().count()),
            Tab::Changes => format!(" {}", d.changed_files),
            Tab::Commits => format!(" {}", d.commits.total_count),
            Tab::Checks => d.rollup().map(|r| format!(" {}", r.contexts.iter().count())).unwrap_or_default(),
            Tab::Actions => app.current_runs().map(|e| format!(" {}", e.runs.iter().filter(|r| r.head_sha == e.sha).count())).unwrap_or_default(),
            Tab::Reviews => format!(" {}", d.reviews.iter().filter(|r| r.state != "COMMENTED" || !clean_body(&r.body).trim().is_empty()).count() + d.review_threads.iter().count()),
            Tab::Stack => String::new(),
        }
    };
    let ci = d.and_then(|d| d.rollup()).map(|r| r.state.clone());
    // The worst outcome among the runs, for the Actions tab's icon.
    let worst = app.current_runs().and_then(|e| {
        let outcomes = e.runs.iter().filter(|r| r.head_sha == e.sha).map(|r| rest_outcome(r.status(), r.conclusion.as_deref()));
        outcomes.min_by_key(|o| outcome_rank(o))
    });
    let counts: Vec<String> = Tab::ALL.into_iter().map(count).collect();
    let badges: Vec<Option<String>> = Tab::ALL.into_iter().map(|t| app.tab_badge(t)).collect();
    // Eight tabs don't fit a narrow pane: shorten the inactive ones' names, then drop them.
    let label_of = |i: usize, t: Tab, level: u8| -> String {
        let name = match level {
            _ if t == app.tab => t.label(),
            0 => t.label(),
            1 => t.short_label(),
            _ => "",
        };
        let mut l = if name.is_empty() { format!(" {}{} ", i + 1, counts[i]) } else { format!(" {} {name}{} ", i + 1, counts[i]) };
        if (t == Tab::Checks && ci.is_some()) || (t == Tab::Actions && worst.is_some()) {
            l.push_str("✓ ");
        }
        l
    };
    let room = area.width.saturating_sub(9) as usize;
    let fits = |level: u8| Tab::ALL.into_iter().enumerate().map(|(i, t)| label_of(i, t, level).width() + badges[i].as_ref().map_or(0, |b| b.width() + 1)).sum::<usize>() <= room;
    let level = (0..2).find(|&l| fits(l)).unwrap_or(2);
    let mut x = area.x;
    let mut spans = Vec::new();
    for (i, t) in Tab::ALL.into_iter().enumerate() {
        let active = t == app.tab;
        let name = match level {
            _ if active => t.label(),
            0 => t.label(),
            1 => t.short_label(),
            _ => "",
        };
        let mut label = if name.is_empty() { format!(" {}{} ", i + 1, counts[i]) } else { format!(" {} {name}{} ", i + 1, counts[i]) };
        if t == Tab::Checks && ci.is_some() {
            label.push_str(&ci_icon(ci.as_deref()).content);
            label.push(' ');
        }
        if t == Tab::Actions && worst.is_some() {
            label.push_str(&ci_icon(worst.as_deref()).content);
            label.push(' ');
        }
        // ` 2 Conversation 7 +2 `: the badge is its own click target (the first new entry).
        let badge = badges[i].as_ref().map(|b| format!("{b} "));
        let (w, bw) = (label.width() as u16, badge.as_ref().map_or(0, |b| b.width() as u16));
        if x + w + bw > area.right() {
            break;
        }
        let style = if active { Style::new().fg(Color::Black).bg(ACCENT).bold() } else { Style::new().fg(Color::Gray) };
        spans.push(Span::styled(label, style));
        app.hits.buttons.push((Rect::new(x, area.y, w, 1), Action::SetTab(t)));
        x += w;
        if let Some(b) = badge {
            let bstyle = if active { style } else { Style::new().fg(ACCENT).bold() };
            spans.push(Span::styled(b, bstyle));
            app.hits.buttons.push((Rect::new(x, area.y, bw, 1), Action::OpenNew(t)));
            x += bw;
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    let toggle = if app.info_collapsed() { " ▾ info " } else { " ▴ info " };
    let tw = toggle.width() as u16;
    if x + tw < area.right() {
        let r = Rect::new(area.right() - tw, area.y, tw, 1);
        f.render_widget(Paragraph::new(toggle).style(Style::new().fg(DIM)), r);
        app.hits.buttons.push((r, Action::ToggleInfo));
    }
}

fn push_wrapped(rows: &mut Vec<DetailRow>, line: Line<'static>, width: usize, indent: &str, link: Option<String>) {
    for l in wrap(line, width, indent) {
        rows.push((l, full_row_link(&link)));
    }
}

fn section(rows: &mut Vec<DetailRow>, title: &str, width: usize) {
    gap(rows);
    let label = format!("── {title} ");
    let fill = width.saturating_sub(label.width());
    rows.push((Line::from(vec![Span::styled(label, Style::new().fg(ACCENT).bold()), Span::styled("─".repeat(fill), Style::new().fg(DIM))]), Vec::new()));
}

fn summary_lines(p: &PrSummary, w: usize, rows: &mut Vec<DetailRow>) {
    push_wrapped(rows, Line::styled(p.title.clone(), Style::new().bold()), w, "", None);
    rows.push((
        Line::from(vec![
            state_badge(&p.state, p.is_draft),
            Span::raw(" "),
            Span::styled(format!("by {} · updated {} ago", login_of(&p.author), ago(&p.updated_at)), Style::new().fg(Color::Gray)),
        ]),
        Vec::new(),
    ));
}

/// PR info header, shown above the tabs.
fn info_lines(d: &PrDetail, w: usize, rows: &mut Vec<DetailRow>) {
    let gray = Style::new().fg(Color::Gray);
    push_wrapped(rows, Line::styled(d.title.clone(), Style::new().bold()), w, "", None);
    let mut l = vec![state_badge(&d.state, d.is_draft), Span::raw(" ")];
    l.push(Span::styled(login_of(&d.author).to_string(), Style::new().fg(Color::Blue).bold()));
    l.push(Span::styled(format!(" opened {} ago · updated {} ago", ago(&d.created_at), ago(&d.updated_at)), gray));
    push_wrapped(rows, Line::from(l), w, "", None);
    // Tagged (a zero-width link no click can hit) so the collapsed header and the stack strip
    // find it even when it wraps.
    let arrow = Line::from(vec![
        Span::styled(d.base_ref_name.clone(), Style::new().fg(Color::Magenta)),
        Span::styled(" ← ", gray),
        Span::styled(d.head_ref_name.clone(), Style::new().fg(Color::Magenta)),
        Span::raw("   "),
        Span::styled(format!("+{}", d.additions), Style::new().fg(Color::Green)),
        Span::raw(" "),
        Span::styled(format!("−{}", d.deletions), Style::new().fg(Color::Red)),
        Span::styled(format!(" · {} files", d.changed_files), gray),
    ]);
    for l in wrap(arrow, w, "") {
        rows.push((l, vec![(0, 0, ARROW_LINK.into())]));
    }
    gap(rows);

    let field = |name: &str| Span::styled(format!("{name:<11}"), Style::new().fg(DIM));
    let labels: Vec<_> = d.labels.iter().collect();
    if !labels.is_empty() {
        let mut l = vec![field("Labels")];
        for lb in labels {
            let bg = u32::from_str_radix(&lb.color, 16).map(|c| Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)).unwrap_or(Color::Gray);
            l.push(Span::styled(format!(" {} ", lb.name), Style::new().fg(Color::Black).bg(bg)));
            l.push(Span::raw(" "));
        }
        push_wrapped(rows, Line::from(l), w, "           ", None);
    }
    let assignees: Vec<_> = d.assignees.iter().map(|a| a.login.clone()).collect();
    if !assignees.is_empty() {
        push_wrapped(rows, Line::from(vec![field("Assignees"), Span::raw(assignees.join(", "))]), w, "           ", None);
    }
    let reviewers: Vec<_> = d.review_requests.iter().filter_map(|r| r.requested_reviewer.as_ref()).map(|r| r.display()).collect();
    if !reviewers.is_empty() {
        push_wrapped(rows, Line::from(vec![field("Awaiting"), Span::styled(reviewers.join(", "), Style::new().fg(Color::Yellow))]), w, "           ", None);
    }
    rows.push((Line::from(vec![field("Review"), review_span(d.review_decision.as_deref())]), Vec::new()));
    let ci = match d.rollup() {
        Some(r) => vec![field("Checks"), ci_icon(Some(&r.state)), Span::styled(format!(" {}", r.state.to_lowercase()), gray)],
        None => vec![field("Checks"), Span::styled("none", gray)],
    };
    rows.push((Line::from(ci), Vec::new()));

    let (merge_icon, merge_text, merge_color) = merge_status(d);
    rows.push((Line::from(vec![field("Merge"), Span::styled(format!("{merge_icon} {merge_text}"), Style::new().fg(merge_color))]), Vec::new()));
    let methods: Vec<&str> = [
        (d.repository.squash_merge_allowed, "squash"),
        (d.repository.merge_commit_allowed, "merge commit"),
        (d.repository.rebase_merge_allowed, "rebase"),
    ]
    .into_iter()
    .filter_map(|(ok, n)| ok.then_some(n))
    .collect();
    let mut m = vec![field("Methods"), Span::styled(methods.join(" · "), gray)];
    if let Some(p) = &d.repository.viewer_permission {
        m.push(Span::styled(format!("   (you: {})", p.to_lowercase()), Style::new().fg(DIM)));
    }
    rows.push((Line::from(m), Vec::new()));
}

fn description_lines(d: &PrDetail, w: usize, rows: &mut Vec<DetailRow>, img: &mut Imgs) {
    if clean_body(&d.body).trim().is_empty() {
        rows.push((Line::styled("No description provided.", Style::new().fg(DIM).italic()), Vec::new()));
    } else {
        body_lines(&d.body, &d.body_html, w, "", rows, img);
        end_marker(rows);
    }
}

fn review_state_span(state: &str) -> Span<'static> {
    match state {
        "APPROVED" => Span::styled("✓ approved", Style::new().fg(Color::Green).bold()),
        "CHANGES_REQUESTED" => Span::styled("± requested changes", Style::new().fg(Color::Red).bold()),
        "DISMISSED" => Span::styled("reviewed (dismissed)", Style::new().fg(DIM)),
        "PENDING" => Span::styled("pending review", Style::new().fg(DIM)),
        _ => Span::styled("commented", Style::new().fg(Color::Gray)),
    }
}

/// What the open PR's visit knows: the new entries and the baseline they're new against.
#[derive(Clone, Copy)]
struct News<'a> {
    marks: &'a NewMarks,
    since: &'a Snapshot,
}

/// How a timeline entry is marked: new (with the divider's count before the first one).
#[derive(Clone, Copy, Default)]
struct Mark {
    new: bool,
    divider: Option<usize>,
}

/// Hands out the divider to the first new entry drawn.
struct Divider(Option<usize>);

impl Divider {
    fn new(n: usize) -> Self {
        Self((n > 0).then_some(n))
    }

    fn mark(&mut self, new: bool) -> Mark {
        Mark { new, divider: if new { self.0.take() } else { None } }
    }
}

/// `── 2 new since you last looked ────`; clicking it jumps to the next new entry.
fn new_divider(rows: &mut Vec<DetailRow>, n: usize, w: usize) {
    let label = format!("── {n} new since you last looked ");
    let fill = w.saturating_sub(label.width());
    rows.push((
        Line::from(vec![Span::styled(label, Style::new().fg(NEW).bold()), Span::styled("─".repeat(fill), Style::new().fg(NEW))]),
        vec![(0, u16::MAX, "#nextnew".into())],
    ));
}

/// The ` · NEW` word after an entry's header.
fn new_word() -> [Span<'static>; 2] {
    [Span::styled(" · ", Style::new().fg(DIM)), Span::styled("NEW", Style::new().fg(NEW).bold())]
}

/// Tag the row at `at` as a `NEW` entry for `n`/`p` (a zero-width link no click can hit).
fn tag_new(rows: &mut [DetailRow], at: usize) {
    if let Some((_, links)) = rows.get_mut(at) {
        links.push((0, 0, NEW_LINK.into()));
    }
}

/// One timeline entry: a header line and its rendered body. A new one has a yellow bar and
/// the word `NEW`.
#[allow(clippy::too_many_arguments)]
fn entry(rows: &mut Vec<DetailRow>, w: usize, author: &str, what: Span<'static>, ts: &str, body: &str, html: &str, img: &mut Imgs, mark: Mark) {
    gap(rows);
    if let Some(n) = mark.divider {
        new_divider(rows, n, w);
    }
    let bar = Style::new().fg(if mark.new { NEW } else { ACCENT });
    let mut head = vec![
        Span::styled("▍", bar),
        Span::styled(author.to_string(), Style::new().fg(Color::Blue).bold()),
        Span::raw(" "),
        what,
        Span::styled(format!(" · {} ago", ago(ts)), Style::new().fg(DIM)),
    ];
    if mark.new {
        head.extend(new_word());
    }
    let at = rows.len();
    push_wrapped(rows, Line::from(head), w, "▍ ", None);
    if mark.new {
        tag_new(rows, at);
    }
    if !clean_body(body).trim().is_empty() {
        body_lines(body, html, w, "▍ ", rows, img);
    }
}

/// ` Merged 2h ago by alice ` over the PR info when it merged or closed since you last looked.
fn state_banner(d: &PrDetail, before: &str, w: usize) -> Option<Line<'static>> {
    if before != "OPEN" || d.state == "OPEN" {
        return None;
    }
    let (what, ev, color) = match d.state.as_str() {
        "MERGED" => ("Merged", d.last_event("MergedEvent"), Color::Magenta),
        _ => ("Closed", d.last_event("ClosedEvent"), Color::Red),
    };
    let mut text = format!("── {what}");
    if let Some(TimelineEvent { actor, created_at, .. }) = ev {
        text.push_str(&format!(" {} ago by {}", ago(created_at), login_of(actor)));
    }
    text.push_str(" since you last looked ");
    let fill = w.saturating_sub(text.width());
    Some(Line::from(vec![Span::styled(text, Style::new().fg(color).bold()), Span::styled("─".repeat(fill), Style::new().fg(color))]))
}

fn end_marker(rows: &mut Vec<DetailRow>) {
    gap(rows);
    rows.push((Line::styled("— end —", Style::new().fg(DIM)).centered(), Vec::new()));
}

/// Comments plus review verdicts, oldest first (inline review comments live in Reviews).
fn conversation_lines(d: &PrDetail, w: usize, rows: &mut Vec<DetailRow>, img: &mut Imgs, news: Option<News>) {
    enum Item<'a> {
        Comment(&'a crate::github::Comment),
        Review(&'a crate::github::Review),
    }
    let empty = NewMarks::default();
    let marks = news.map_or(&empty, |n| n.marks);
    let mut items: Vec<(&str, Item, bool)> =
        d.comments.iter().enumerate().map(|(i, c)| (c.created_at.as_str(), Item::Comment(c), marks.comments.contains(&i))).collect();
    for (i, r) in d.reviews.iter().enumerate() {
        // Empty "COMMENTED" reviews are just containers for inline comments.
        if r.state == "COMMENTED" && clean_body(&r.body).trim().is_empty() {
            continue;
        }
        items.push((r.submitted_at.as_deref().unwrap_or(""), Item::Review(r), marks.reviews.contains(&i)));
    }
    items.sort_by(|a, b| a.0.cmp(b.0));
    if items.is_empty() {
        rows.push((Line::styled("No comments yet. Press c to add one.", Style::new().fg(DIM).italic()), Vec::new()));
        return;
    }
    let mut divider = Divider::new(items.iter().filter(|i| i.2).count());
    for (ts, item, new) in items {
        let mark = divider.mark(new);
        match item {
            Item::Comment(c) => entry(rows, w, login_of(&c.author), Span::styled("commented", Style::new().fg(Color::Gray)), ts, &c.body, &c.body_html, img, mark),
            Item::Review(r) => entry(rows, w, login_of(&r.author), review_state_span(&r.state), ts, &r.body, &r.body_html, img, mark),
        }
    }
    end_marker(rows);
}

fn reviews_lines(d: &PrDetail, w: usize, rows: &mut Vec<DetailRow>, img: &mut Imgs, news: Option<News>) {
    let empty = NewMarks::default();
    let marks = news.map_or(&empty, |n| n.marks);
    // Latest verdict per reviewer (and whether it's new): a later COMMENTED doesn't override an approval.
    let mut latest: Vec<(String, String, bool)> = Vec::new();
    for (i, r) in d.reviews.iter().enumerate() {
        let who = login_of(&r.author).to_string();
        let new = marks.reviews.contains(&i) && r.state != "COMMENTED";
        match latest.iter_mut().find(|(a, ..)| *a == who) {
            Some(e) if r.state != "COMMENTED" => *e = (who, r.state.clone(), new),
            Some(_) => {}
            None => latest.push((who, r.state.clone(), new)),
        }
    }
    let pending: Vec<_> = d.review_requests.iter().filter_map(|r| r.requested_reviewer.as_ref()).map(|r| r.display()).collect();
    rows.push((Line::from(vec![Span::styled("Reviewers", Style::new().bold()), Span::raw("  "), review_span(d.review_decision.as_deref())]), Vec::new()));
    if latest.is_empty() && pending.is_empty() {
        rows.push((Line::styled("  No reviews yet.", Style::new().fg(DIM).italic()), Vec::new()));
    }
    for (who, state, new) in &latest {
        let mut l = vec![Span::raw("  "), Span::styled(format!("{who:<28}"), Style::new().fg(Color::Blue)), review_state_span(state)];
        if *new {
            l.push(Span::raw("  "));
            l.push(Span::styled("NEW", Style::new().fg(NEW).bold()));
        }
        rows.push((Line::from(l), Vec::new()));
    }
    for who in &pending {
        rows.push((Line::from(vec![Span::raw("  "), Span::styled(format!("{who:<28}"), Style::new().fg(Color::Blue)), Span::styled("◔ awaiting review", Style::new().fg(Color::Yellow))]), Vec::new()));
    }

    let with_body: Vec<_> = d.reviews.iter().enumerate().filter(|(_, r)| !clean_body(&r.body).trim().is_empty()).collect();
    let threads: Vec<_> = d.review_threads.iter().enumerate().collect();
    // One divider, before the first new entry drawn (summaries come before threads).
    let n_new = with_body.iter().filter(|(i, _)| marks.reviews.contains(i)).count() + marks.threads.len();
    let mut divider = Divider::new(n_new);
    if !with_body.is_empty() {
        section(rows, &format!("Review summaries ({})", with_body.len()), w);
        for (i, r) in with_body {
            let mark = divider.mark(marks.reviews.contains(&i));
            entry(rows, w, login_of(&r.author), review_state_span(&r.state), r.submitted_at.as_deref().unwrap_or(""), &r.body, &r.body_html, img, mark);
        }
    }

    let open = threads.iter().filter(|(_, t)| !t.is_resolved).count();
    section(rows, &format!("Inline threads ({} · {open} unresolved)", threads.len()), w);
    if threads.is_empty() {
        rows.push((Line::styled("No inline review comments.", Style::new().fg(DIM).italic()), Vec::new()));
    }
    // Unresolved threads first, then by file.
    let mut threads = threads;
    threads.sort_by(|(_, a), (_, b)| (a.is_resolved, &a.path, a.line).cmp(&(b.is_resolved, &b.path, b.line)));
    for (ti, t) in threads {
        gap(rows);
        let loc = match t.line {
            Some(l) => format!("{}:{l}", t.path),
            None => t.path.clone(),
        };
        let (mark, color) = if t.is_resolved { ("✓ resolved", Color::Green) } else { ("● open", Color::Yellow) };
        push_wrapped(
            rows,
            Line::from(vec![Span::styled(format!("{mark}  "), Style::new().fg(color)), Span::styled(loc, Style::new().fg(Color::Magenta).bold())]),
            w,
            "   ",
            None,
        );
        for (ci, c) in t.comments.iter().enumerate() {
            let mark = divider.mark(marks.threads.contains(&(ti, ci)));
            entry(rows, w, login_of(&c.author), Span::raw(""), &c.created_at, &c.body, &c.body_html, img, mark);
        }
    }
    end_marker(rows);
}

/// `head moved d1e3597 → 9af01c2 (force-push by alice, 5m ago)` when the head was rewritten
/// since you last looked.
fn force_push_banner(d: &PrDetail, since: &Snapshot, w: usize) -> Option<Line<'static>> {
    if since.head.is_empty() || since.head == d.head_ref_oid {
        return None;
    }
    let ev = d.timeline_items.iter().filter(|e| e.typename == "HeadRefForcePushedEvent" && e.created_at.as_str() > since.updated_at.as_str()).last();
    if ev.is_none() && d.commits.total_count > since.commits {
        return None;
    }
    let mut why = "force-push".to_string();
    if let Some(e) = ev {
        why.push_str(&format!(" by {}, {} ago", login_of(&e.actor), ago(&e.created_at)));
    }
    let text = format!("head moved {} → {} ({why})", short_sha(&since.head), short_sha(&d.head_ref_oid));
    Some(Line::from(vec![Span::styled("! ", Style::new().fg(NEW).bold()), Span::styled(truncate(&text, w.saturating_sub(2)), Style::new().fg(NEW).bold())]))
}

fn commits_lines(d: &PrDetail, w: usize, rows: &mut Vec<DetailRow>, news: Option<News>) {
    if let Some(b) = news.and_then(|n| force_push_banner(d, n.since, w)) {
        rows.push((b, Vec::new()));
        gap(rows);
    }
    let empty = NewMarks::default();
    let marks = news.map_or(&empty, |n| n.marks);
    let mut divider = Divider::new(marks.commits.len());
    let shown = d.commits.nodes.iter().flatten().count();
    let mut head = vec![Span::styled(format!("{} commits", d.commits.total_count), Style::new().bold())];
    if d.commits.total_count > shown {
        head.push(Span::styled(format!("  (showing the latest {shown})"), Style::new().fg(DIM)));
    }
    head.push(Span::styled("   click a commit to open it", Style::new().fg(DIM)));
    rows.push((Line::from(head), Vec::new()));
    gap(rows);
    let all: Vec<_> = d.commits.nodes.iter().flatten().map(|n| &n.commit).enumerate().collect();
    for (i, c) in all.into_iter().rev() {
        let link = Some(c.url.clone());
        let mark = divider.mark(marks.commits.contains(&i));
        if let Some(n) = mark.divider {
            new_divider(rows, n, w);
        }
        let headline = truncate(&c.message_headline, w.saturating_sub(if mark.new { 18 } else { 12 }));
        let mut l = vec![
            ci_icon(c.status_check_rollup.as_ref().map(|r| r.state.as_str())),
            Span::raw(" "),
            Span::styled(c.abbreviated_oid.clone(), Style::new().fg(Color::Yellow)),
            Span::raw(" "),
            Span::styled(headline, Style::new().bold()),
        ];
        let mut links = full_row_link(&link);
        if mark.new {
            l.extend(new_word());
            links.push((0, 0, NEW_LINK.into()));
        }
        rows.push((Line::from(l), links));
        let meta = format!("{} · {} ago", c.author(), ago(&c.committed_date));
        if density() == Density::Compact {
            // One line per commit: append the author and age to the headline row.
            if let Some((line, _)) = rows.last_mut() {
                line.spans.push(Span::styled(format!("  {meta}"), Style::new().fg(DIM)));
            }
        } else {
            rows.push((Line::styled(format!("          {meta}"), Style::new().fg(DIM)), full_row_link(&link)));
            if density() == Density::Comfortable {
                rows.push((Line::raw(""), Vec::new()));
            }
        }
    }
}

fn checks_lines(d: &PrDetail, w: usize, rows: &mut Vec<DetailRow>) {
    let Some(rollup) = d.rollup() else {
        rows.push((Line::styled("No checks reported for the head commit.", Style::new().fg(DIM).italic()), Vec::new()));
        return;
    };
    let mut checks: Vec<_> = rollup.contexts.iter().collect();
    let rank = |o: &str| match o {
        "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => 0,
        "PENDING" | "EXPECTED" | "IN_PROGRESS" | "QUEUED" => 1,
        "SUCCESS" => 2,
        _ => 3,
    };
    checks.sort_by_key(|c| (rank(&c.outcome()), c.label()));
    let (mut pass, mut fail, mut pend, mut other) = (0, 0, 0, 0);
    for c in &checks {
        match rank(&c.outcome()) {
            0 => fail += 1,
            1 => pend += 1,
            2 => pass += 1,
            _ => other += 1,
        }
    }
    rows.push((
        Line::from(vec![
            ci_icon(Some(&rollup.state)),
            Span::raw(" "),
            Span::styled(format!("{pass} passed"), Style::new().fg(Color::Green)),
            Span::raw(" · "),
            Span::styled(format!("{fail} failed"), Style::new().fg(if fail > 0 { Color::Red } else { DIM })),
            Span::raw(" · "),
            Span::styled(format!("{pend} pending"), Style::new().fg(if pend > 0 { Color::Yellow } else { DIM })),
            Span::raw(" · "),
            Span::styled(format!("{other} skipped/neutral"), Style::new().fg(DIM)),
        ]),
        Vec::new(),
    ));
    rows.push((Line::styled("click a check to open its logs", Style::new().fg(DIM)), Vec::new()));
    gap(rows);
    for c in checks {
        let link = c.url().map(String::from);
        let outcome = c.outcome();
        let label = truncate(&c.label(), w.saturating_sub(18));
        let style = if link.is_some() { Style::new().add_modifier(Modifier::UNDERLINED) } else { Style::new() };
        rows.push((
            Line::from(vec![
                Span::raw("  "),
                ci_icon(Some(&outcome)),
                Span::raw(" "),
                Span::styled(label, style),
                Span::styled(format!("  {}", outcome.to_lowercase()), Style::new().fg(DIM)),
            ]),
            full_row_link(&link),
        ));
    }
}

fn file_status(s: &str) -> (&'static str, Color) {
    match s {
        "added" => ("A", Color::Green),
        "removed" => ("D", Color::Red),
        "renamed" => ("R", Color::Blue),
        "copied" => ("C", Color::Blue),
        _ => ("M", Color::Yellow),
    }
}

/// One file's header bar and patch with old/new line numbers.
fn file_diff_lines(file: &FileDiff, w: usize, rows: &mut Vec<DetailRow>) {
    let (letter, color) = file_status(&file.status);
    let name = match &file.previous_filename {
        Some(prev) => format!("{prev} → {}", file.filename),
        None => file.filename.clone(),
    };
    let counts = format!("+{} −{} ", file.additions, file.deletions);
    let name = truncate(&name, w.saturating_sub(counts.width() + 4));
    let pad = w.saturating_sub(name.width() + counts.width() + 3);
    let bar = Style::new().bg(Color::Indexed(237));
    rows.push((
        Line::from(vec![
            Span::styled(format!(" {letter} "), bar.fg(color).bold()),
            Span::styled(name, bar.fg(Color::White).bold()),
            Span::styled(" ".repeat(pad), bar),
            Span::styled(counts, bar.fg(Color::Gray)),
        ]),
        Vec::new(),
    ));
    let Some(patch) = &file.patch else {
        rows.push((Line::styled("   Binary file, or the diff is too large to show here — open the PR in the browser.", Style::new().fg(DIM).italic()), Vec::new()));
        return;
    };
    let num_w = 5;
    let code_w = w.saturating_sub(num_w * 2 + 3);
    let (mut old, mut new) = (0u64, 0u64);
    for raw in patch.lines() {
        let line = raw.replace('\t', "    ");
        if let Some(rest) = line.strip_prefix("@@") {
            // @@ -a,b +c,d @@ context
            let mut parts = rest.split_whitespace();
            let parse = |p: Option<&str>| p.and_then(|s| s.get(1..)?.split(',').next()?.parse::<u64>().ok()).unwrap_or(1);
            old = parse(parts.next());
            new = parse(parts.next());
            gap(rows);
            rows.push((Line::styled(truncate(&line, w), Style::new().fg(ACCENT).add_modifier(Modifier::DIM)), Vec::new()));
            continue;
        }
        let (sign, code) = line.split_at(line.chars().next().map_or(0, char::len_utf8));
        let (o, n, style) = match sign {
            "+" => (String::new(), num(new, &mut new), Style::new().fg(Color::Green)),
            "-" => (num(old, &mut old), String::new(), Style::new().fg(Color::Red)),
            "\\" => (String::new(), String::new(), Style::new().fg(DIM)),
            _ => (num(old, &mut old), num(new, &mut new), Style::new()),
        };
        rows.push((
            Line::from(vec![
                Span::styled(format!("{o:>num_w$} {n:>num_w$} │"), Style::new().fg(DIM)),
                Span::styled(format!("{sign}{}", truncate(code, code_w)), style),
            ]),
            Vec::new(),
        ));
    }
    end_marker(rows);
}

/// Format a line number and advance the counter.
fn num(cur: u64, counter: &mut u64) -> String {
    *counter = cur + 1;
    cur.to_string()
}

fn merge_status(d: &PrDetail) -> (&'static str, String, Color) {
    if d.state == "MERGED" {
        return ("◆", "Merged".into(), Color::Magenta);
    }
    if d.state == "CLOSED" {
        return ("✕", "Closed".into(), Color::Red);
    }
    if d.mergeable == "CONFLICTING" {
        return ("✗", "Has merge conflicts".into(), Color::Red);
    }
    match d.merge_state_status.as_deref() {
        Some("CLEAN") => ("✓", "Ready to merge".into(), Color::Green),
        Some("HAS_HOOKS") => ("✓", "Ready to merge (pre-receive hooks)".into(), Color::Green),
        Some("UNSTABLE") => ("!", "Mergeable, but some checks are failing".into(), Color::Yellow),
        Some("BLOCKED") => ("⊘", "Blocked by branch protection (reviews or required checks)".into(), Color::Yellow),
        Some("BEHIND") => ("↓", "Head branch is behind the base branch".into(), Color::Yellow),
        Some("DIRTY") => ("✗", "Merge commit cannot be cleanly created".into(), Color::Red),
        Some("DRAFT") => ("◌", "Draft — mark ready for review to merge".into(), DIM),
        _ => ("…", "Checking mergeability…".into(), DIM),
    }
}

fn full_row_link(link: &Option<String>) -> Vec<(u16, u16, String)> {
    link.iter().map(|u| (0, u16::MAX, u.clone())).collect()
}

/// Inline images for the row builders: what the store knows, and the placements found.
struct Imgs<'a> {
    store: &'a ImageStore,
    out: Vec<ImagePlacement>,
}

impl<'a> Imgs<'a> {
    fn new(store: &'a ImageStore) -> Self {
        Self { store, out: Vec::new() }
    }
}

/// Render a body from GitHub's rendered HTML, falling back to the raw markdown.
fn body_lines(markdown: &str, html: &str, w: usize, indent: &str, rows: &mut Vec<DetailRow>, img: &mut Imgs) {
    if html.trim().is_empty() || !html_lines(html, w, indent, rows, img) {
        markdown_lines(&clean_body(markdown), w, indent, rows);
    }
}

/// The `src` and alt text of an html2text line that holds one image and nothing else (the
/// GitHub paragraph with a lone screenshot, often wrapped in a link to itself).
fn lone_image(tl: &html2text::render::TaggedLine<Vec<html2text::render::RichAnnotation>>) -> Option<(String, String)> {
    use html2text::render::RichAnnotation as A;
    let mut found: Option<(String, String)> = None;
    for ts in tl.tagged_strings() {
        let src = ts.tag.iter().find_map(|a| if let A::Image(src) = a { Some(src) } else { None });
        match (src, &mut found) {
            (Some(src), None) => found = Some((src.clone(), ts.s.trim().to_string())),
            (Some(src), Some((f, alt))) if f == src => alt.push_str(ts.s.trim()),
            _ if ts.s.trim().is_empty() => {}
            _ => return None,
        }
    }
    found
}

/// The rows for an image alone on its line, or `None` to keep the `▣ alt` label: images are
/// off, it's a badge (`width`/`height` under 64 px, or SVG), or it was skipped or failed.
fn image_rows(src: &str, alt: &str, attrs: &images::ImgAttrs, indent: &str, width: usize, base: usize, img: &mut Imgs) -> Option<Vec<DetailRow>> {
    let small = |v: Option<u32>| v.is_some_and(|v| v < images::MIN_PX);
    let svg = images::looks_svg(src) || attrs.canonical.as_deref().is_some_and(images::looks_svg);
    let (key, url, _) = images::canonical(src);
    if !img.store.inline_ok() || small(attrs.width) || small(attrs.height) || svg || url.is_none() {
        return None;
    }
    let bar = || Span::styled(indent.to_string(), Style::new().fg(ACCENT));
    let link = vec![(0, u16::MAX, format!("#image:{key}"))];
    let x = indent.width() as u16;
    let alt = if alt.is_empty() { "image".to_string() } else { alt.to_string() };
    let mut placement = ImagePlacement { key: key.clone(), row: base, rows: 1, cols: width as u16, x, alt: alt.clone() };
    match img.store.look(&key) {
        Look::Ready(dims) => {
            let size = images::fit(dims, img.store.font(), width as u16, img.store.max_rows);
            if size.height == 0 {
                return None;
            }
            (placement.rows, placement.cols) = (size.height, size.width);
            img.out.push(placement);
            Some((0..size.height).map(|_| (Line::from(bar()), link.clone())).collect())
        }
        Look::Pending => {
            img.out.push(placement);
            let label = vec![bar(), Span::styled(format!("▣ {alt}"), Style::new().fg(Color::Magenta)), Span::styled(" (loading…)", Style::new().fg(DIM))];
            Some(vec![(Line::from(label), link)])
        }
        Look::Failed(e) => {
            let note = format!(" (failed: {})", truncate(&e, 60));
            let label = vec![bar(), Span::styled(format!("▣ {alt}"), Style::new().fg(Color::Magenta)), Span::styled(note, Style::new().fg(DIM))];
            Some(vec![(Line::from(label), vec![(0, u16::MAX, key)])])
        }
        Look::Skipped(_) => None,
    }
}

/// Render HTML to styled rows with clickable link ranges. Returns false if it couldn't be parsed.
fn html_lines(html: &str, w: usize, indent: &str, rows: &mut Vec<DetailRow>, img: &mut Imgs) -> bool {
    use html2text::render::RichAnnotation as A;
    // html2text drops the attributes that tell a screenshot from a badge.
    let attrs = images::img_attrs(html);
    // Images with no alt text would otherwise render as nothing; <sup>/<sub> render as ^{…}.
    let html = html.replace("alt=\"\"", "alt=\"image\"");
    let html = ["<sup>", "</sup>", "<sub>", "</sub>"].iter().fold(html, |h, t| h.replace(t, ""));
    let indent_w = indent.width();
    let width = w.saturating_sub(indent_w).max(10);
    let Ok(lines) = html2text::config::rich().lines_from_read(html.as_bytes(), width) else { return false };
    let mut out: Vec<DetailRow> = Vec::new();
    // Blank text, and not an image row (those are blank too, but carry a link).
    let blank = |(l, links): &DetailRow| !links.iter().any(|(_, _, u)| u.starts_with("#image:")) && l.to_string().trim_start_matches(indent).trim().is_empty();
    // The image the previous line was, so its alt text wrapping onto the next line is dropped.
    let mut last_image: Option<String> = None;
    for tl in lines {
        if let Some((src, alt)) = lone_image(&tl) {
            if last_image.as_ref() == Some(&src) {
                continue;
            }
            let a = attrs.get(&src).cloned().unwrap_or_default();
            if let Some(image) = image_rows(&src, &alt, &a, indent, width, rows.len() + out.len(), img) {
                out.extend(image);
                last_image = Some(src);
                continue;
            }
        }
        last_image = None;
        let mut spans = vec![Span::styled(indent.to_string(), Style::new().fg(ACCENT))];
        let mut links = Vec::new();
        let mut x = indent_w;
        let text: String = tl.tagged_strings().map(|t| t.s.as_str()).collect();
        let line_style = if text.starts_with('#') {
            Style::new().fg(ACCENT).bold()
        } else if text.starts_with("> ") {
            Style::new().fg(Color::Gray).italic()
        } else {
            Style::new()
        };
        for ts in tl.tagged_strings() {
            let mut style = line_style;
            let mut link = None;
            let mut content = ts.s.clone();
            for ann in &ts.tag {
                match ann {
                    A::Default => {}
                    A::Link(u) => {
                        style = style.fg(Color::Blue).underlined();
                        link = Some(u.clone());
                    }
                    A::Image(src) => {
                        style = style.fg(Color::Magenta);
                        content = format!("▣ {content}");
                        link = Some(src.clone());
                    }
                    A::Emphasis => style = style.italic(),
                    A::Strong => style = style.bold(),
                    A::Strikeout => style = style.crossed_out(),
                    A::Code | A::Preformat(_) => style = style.fg(Color::Yellow),
                    A::Colour(c) => style = style.fg(Color::Rgb(c.r, c.g, c.b)),
                    _ => {}
                }
            }
            let sw = content.width();
            if let Some(u) = link {
                links.push((x as u16, (x + sw) as u16, u));
            }
            spans.push(Span::styled(content, style));
            x += sw;
        }
        // Collapse runs of blank lines.
        let row = (Line::from(spans), links);
        if blank(&row) && out.last().is_none_or(blank) {
            continue;
        }
        out.push(row);
    }
    while out.last().is_some_and(blank) {
        out.pop();
    }
    rows.extend(out);
    true
}

/// Strip HTML comments and carriage returns from GitHub markdown.
fn clean_body(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("<!--") {
        out.push_str(&rest[..i]);
        match rest[i..].find("-->") {
            Some(j) => rest = &rest[i + j + 3..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    let out = out.replace('\r', "");
    // Collapse runs of blank lines.
    let mut lines: Vec<&str> = Vec::new();
    for l in out.lines() {
        if l.trim().is_empty() && lines.last().is_none_or(|p| p.trim().is_empty()) {
            continue;
        }
        lines.push(l);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Very light markdown styling: headings, quotes, code fences, bullets and @mentions.
fn markdown_lines(body: &str, w: usize, indent: &str, rows: &mut Vec<DetailRow>) {
    let mut in_code = false;
    for raw in body.lines() {
        let t = raw.trim_start();
        if t.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        let mut spans = vec![Span::styled(indent.to_string(), Style::new().fg(ACCENT))];
        if in_code {
            spans.push(Span::styled(format!("  {raw}"), Style::new().fg(Color::Yellow)));
        } else if t.starts_with('#') {
            spans.push(Span::styled(t.trim_start_matches('#').trim().to_string(), Style::new().bold().underlined()));
        } else if let Some(q) = t.strip_prefix('>') {
            spans.push(Span::styled(format!("│ {}", q.trim()), Style::new().fg(Color::Gray).italic()));
        } else {
            let lead = &raw[..raw.len() - t.len()];
            let (bullet, text) = match t.split_at_checked(2) {
                Some(("- " | "* ", rest)) => ("• ", rest),
                _ => ("", t),
            };
            spans.push(Span::raw(format!("{lead}{bullet}")));
            spans.extend(inline_spans(text));
        }
        let cont = format!("{indent}  ");
        push_wrapped(rows, Line::from(spans), w, &cont, None);
    }
}

/// Highlight @mentions, `code` and **bold** inline.
fn inline_spans(text: &str) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let flush = |buf: &mut String, out: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            out.push(Span::raw(std::mem::take(buf)));
        }
    };
    while i < chars.len() {
        let c = chars[i];
        let word_start = i == 0 || chars[i - 1].is_whitespace() || "([".contains(chars[i - 1]);
        if c == '@' && word_start && chars.get(i + 1).is_some_and(|n| n.is_ascii_alphanumeric()) {
            flush(&mut buf, &mut out);
            let mut j = i + 1;
            while j < chars.len() && crate::editor::is_login_char(chars[j]) {
                j += 1;
            }
            out.push(Span::styled(chars[i..j].iter().collect::<String>(), Style::new().fg(ACCENT).bold()));
            i = j;
        } else if c == '`'
            && let Some(end) = chars[i + 1..].iter().position(|&x| x == '`')
        {
            flush(&mut buf, &mut out);
            out.push(Span::styled(chars[i + 1..i + 1 + end].iter().collect::<String>(), Style::new().fg(Color::Yellow)));
            i += end + 2;
        } else if c == '*'
            && chars.get(i + 1) == Some(&'*')
            && let Some(end) = (i + 2..chars.len().saturating_sub(1)).find(|&k| chars[k] == '*' && chars[k + 1] == '*')
        {
            flush(&mut buf, &mut out);
            out.push(Span::styled(chars[i + 2..end].iter().collect::<String>(), Style::new().bold()));
            i = end + 2;
        } else {
            buf.push(c);
            i += 1;
        }
    }
    flush(&mut buf, &mut out);
    out
}

// ---------- modals ----------

fn draw_scopes(f: &mut Frame, app: &mut App) {
    let Some(Modal::Scopes { filter, sel }) = &app.modal else { return };
    let matches = scope_matches(&app.scopes, filter, app.repo.is_some());
    let area = centered(f.area(), 64, (matches.len() as u16 + 7).min(f.area().height * 2 / 3));
    f.render_widget(Clear, area);
    let block = modal_block("Choose scope");
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hits.modal = Some(area);
    let [input, sep, list, hint] = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let input_line = if filter.is_empty() {
        Line::from(vec![Span::styled(" › ", Style::new().fg(ACCENT)), Span::styled("type to filter…", Style::new().fg(DIM))])
    } else {
        Line::from(vec![Span::styled(" › ", Style::new().fg(ACCENT)), Span::raw(filter.clone()), Span::styled("▏", Style::new().fg(ACCENT))])
    };
    f.render_widget(Paragraph::new(input_line), input);
    f.render_widget(Paragraph::new("─".repeat(sep.width as usize)).style(Style::new().fg(DIM)), sep);

    let h = list.height as usize;
    let offset = if *sel >= h { sel + 1 - h } else { 0 };
    let mut lines = Vec::new();
    for (row, (vi, &si)) in matches.iter().enumerate().skip(offset).take(h).enumerate() {
        let s = &app.scopes[si];
        let selected = row + offset == *sel;
        let mark = if si == app.scope { "✓ " } else { "  " };
        let name_w = 28.min(list.width as usize / 2);
        let line = Line::from(vec![
            Span::styled(if selected { "▌" } else { " " }, Style::new().fg(ACCENT)),
            Span::styled(mark, Style::new().fg(Color::Green)),
            Span::styled(format!("{:<name_w$}", truncate(&s.name, name_w)), Style::new().bold()),
            Span::styled(format!(" {}", s.qualifier), Style::new().fg(DIM)),
        ]);
        lines.push(if selected { line.style(Style::new().bg(Color::Indexed(238))) } else { line });
        app.hits.modal_items.push((Rect::new(list.x, list.y + row as u16, list.width, 1), vi));
    }
    if matches.is_empty() {
        lines.push(Line::styled("  No matching scopes", Style::new().fg(DIM)));
    }
    f.render_widget(Paragraph::new(lines), list);
    f.render_widget(Paragraph::new(Line::styled(" ↑↓ move · Enter / click select · Esc close", Style::new().fg(DIM))), hint);
}

fn draw_repos(f: &mut Frame, app: &mut App) {
    let Some(Modal::Repos { filter, sel }) = &app.modal else { return };
    let (filter, sel) = (filter.clone(), *sel);
    let items = repo_items(app, &filter);
    let area = centered(f.area(), 76, (items.len() as u16 + 7).min(f.area().height * 2 / 3));
    f.render_widget(Clear, area);
    let block = modal_block("Choose repository");
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hits.modal = Some(area);
    let [input, sep, list, hint] = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let input_line = if filter.is_empty() {
        Line::from(vec![Span::styled(" › ", Style::new().fg(ACCENT)), Span::styled("type to search repositories (owner/name)…", Style::new().fg(DIM))])
    } else {
        Line::from(vec![Span::styled(" › ", Style::new().fg(ACCENT)), Span::raw(filter.clone()), Span::styled("▏", Style::new().fg(ACCENT))])
    };
    f.render_widget(Paragraph::new(input_line), input);
    f.render_widget(Paragraph::new("─".repeat(sep.width as usize)).style(Style::new().fg(DIM)), sep);

    // Map the selectable index to a visual row, then scroll to keep it visible.
    let mut sel_row = 0;
    let mut k = 0;
    for (row, it) in items.iter().enumerate() {
        if it.value().is_some() {
            if k == sel {
                sel_row = row;
            }
            k += 1;
        }
    }
    let h = list.height as usize;
    let offset = if sel_row >= h { sel_row + 1 - h } else { 0 };
    let w = list.width as usize;
    let more = app.total > app.prs.len();
    let count_text = |c: usize| if more { format!("{c}+ PRs") } else { format!("{c} PRs") };
    let mut lines = Vec::new();
    let mut k = items.iter().take(offset).filter(|i| i.value().is_some()).count();
    for (row, it) in items.iter().enumerate().skip(offset).take(h) {
        let y = list.y + (row - offset) as u16;
        let selectable = it.value().is_some();
        let selected = selectable && k == sel;
        let active = it.value().is_some_and(|v| match (&v, &app.repo) {
            (None, None) => true,
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        });
        let lead = vec![
            Span::styled(if selected { "▌" } else { " " }, Style::new().fg(ACCENT)),
            Span::styled(if active { "✓ " } else { "  " }, Style::new().fg(Color::Green)),
        ];
        let (mut spans, right): (Vec<Span>, String) = match it {
            RepoItem::All { count } => (vec![Span::styled("All repositories", Style::new().bold())], if *count > 0 { count_text(*count) } else { String::new() }),
            RepoItem::ThisPr(r, c) => (
                vec![Span::styled("This PR's repo  ", Style::new().fg(Color::Yellow)), Span::styled(r.clone(), Style::new().bold())],
                c.map(count_text).unwrap_or_default(),
            ),
            RepoItem::Header(t) => (vec![Span::styled(t.clone(), Style::new().fg(ACCENT).bold())], String::new()),
            RepoItem::Repo { nwo, count, private, pushed } => {
                let (owner, name) = nwo.split_once('/').unwrap_or(("", nwo));
                let mut v = vec![Span::styled(format!("{owner}/"), Style::new().fg(DIM)), Span::styled(name.to_string(), Style::new().bold())];
                if *private {
                    v.push(Span::styled("  private", Style::new().fg(DIM)));
                }
                let right = match (count, pushed) {
                    (Some(c), _) => count_text(*c),
                    (None, Some(p)) => format!("pushed {} ago", ago(p)),
                    _ => String::new(),
                };
                (v, right)
            }
            RepoItem::UseTyped(t) => (vec![Span::raw("Use "), Span::styled(format!("“{t}”"), Style::new().bold())], String::new()),
            RepoItem::Note(n) => (vec![Span::styled(n.clone(), Style::new().fg(DIM).italic())], String::new()),
        };
        let used: usize = 3 + spans.iter().map(|s| s.width()).sum::<usize>();
        let pad = w.saturating_sub(used + right.width() + 1);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(right, Style::new().fg(DIM)));
        let mut all = if matches!(it, RepoItem::Header(_) | RepoItem::Note(_)) { vec![Span::raw(" ")] } else { lead };
        all.extend(spans);
        let line = Line::from(all);
        lines.push(if selected { line.style(Style::new().bg(Color::Indexed(238))) } else { line });
        if selectable {
            app.hits.modal_items.push((Rect::new(list.x, y, list.width, 1), k));
            k += 1;
        }
    }
    f.render_widget(Paragraph::new(lines), list);
    f.render_widget(Paragraph::new(Line::styled(" ↑↓ move · Enter / click select · e on a PR toggles its repo · Esc close", Style::new().fg(DIM))), hint);
}

fn draw_menu(f: &mut Frame, app: &mut App) {
    let Some(Modal::Menu { entries, sel, .. }) = &app.modal else { return };
    let title = app.selected_pr().map(|p| format!("Actions · #{}", p.number)).unwrap_or_else(|| "Actions".into());
    let area = centered(f.area(), 58, entries.len() as u16 + 4);
    f.render_widget(Clear, area);
    let block = modal_block(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hits.modal = Some(area);
    let mut lines = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let selected = i == *sel;
        let enabled = e.disabled.is_none();
        let mut spans = vec![
            Span::styled(if selected { "▌" } else { " " }, Style::new().fg(ACCENT)),
            Span::styled(format!(" {} ", e.key), if enabled { Style::new().fg(Color::Black).bg(ACCENT) } else { Style::new().fg(DIM) }),
            Span::raw(" "),
            Span::styled(e.label.clone(), if enabled { Style::new().bold() } else { Style::new().fg(DIM) }),
        ];
        if let Some(why) = &e.disabled {
            spans.push(Span::styled(format!("  ({why})"), Style::new().fg(DIM).italic()));
        }
        let line = Line::from(spans);
        lines.push(if selected { line.style(Style::new().bg(Color::Indexed(238))) } else { line });
        app.hits.modal_items.push((Rect::new(inner.x, inner.y + i as u16, inner.width, 1), i));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(" press a key, click, or ↑↓ + Enter · Esc close", Style::new().fg(DIM)));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_confirm(f: &mut Frame, app: &mut App) {
    let Some(Modal::Confirm { title, message, .. }) = &app.modal else { return };
    let msg_lines: Vec<Line> = message.lines().map(|l| Line::raw(l.to_string()).centered()).collect();
    let area = centered(f.area(), 64, msg_lines.len() as u16 + 6);
    f.render_widget(Clear, area);
    let block = modal_block(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hits.modal = Some(area);
    let [msg, _, buttons] = Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    f.render_widget(Paragraph::new(msg_lines).wrap(ratatui::widgets::Wrap { trim: false }), msg);
    let yes = "  Yes (y)  ";
    let no = "  No (n)  ";
    let total = (yes.width() + no.width() + 4) as u16;
    let x = buttons.x + buttons.width.saturating_sub(total) / 2;
    let yes_r = Rect::new(x, buttons.y, yes.width() as u16, 1);
    let no_r = Rect::new(x + yes.width() as u16 + 4, buttons.y, no.width() as u16, 1);
    f.render_widget(Paragraph::new(yes).style(Style::new().fg(Color::Black).bg(Color::Green).bold()), yes_r);
    f.render_widget(Paragraph::new(no).style(Style::new().fg(Color::Black).bg(Color::Gray)), no_r);
    app.hits.buttons.push((yes_r, Action::ConfirmYes));
    app.hits.buttons.push((no_r, Action::ModalCancel));
}

fn draw_composer(f: &mut Frame, app: &mut App) {
    let candidates = app.mention_candidates();
    let Some(Modal::Composer(c)) = app.modal.as_mut() else { return };
    let fa = f.area();
    let area = centered(fa, (fa.width * 4 / 5).max(60), (fa.height * 7 / 10).max(12));
    f.render_widget(Clear, area);
    let (verb, color) = match c.kind {
        ComposeKind::Comment => ("Comment on", ACCENT),
        ComposeKind::Approve => ("Approve", Color::Green),
        ComposeKind::RequestChanges => ("Request changes on", Color::Red),
    };
    let block = modal_block(&format!("{verb} {}", c.pr_label)).border_style(Style::new().fg(color));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hits.modal = Some(area);
    let [editor_area, sep, bottom] = Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    let editor_area = Rect::new(editor_area.x + 1, editor_area.y, editor_area.width.saturating_sub(2), editor_area.height);

    let rows = c.editor.visual_rows(editor_area.width as usize);
    c.editor.ensure_visible(&rows, editor_area.height as usize);
    let (cy, cx) = c.editor.cursor_visual(&rows);
    let lines: Vec<Line> = if c.editor.is_blank() && rows.len() == 1 {
        vec![Line::styled("Write a comment… Markdown supported. Type @ to mention people or bots (e.g. @claude, @copilot).", Style::new().fg(DIM))]
    } else {
        rows.iter().skip(c.editor.scroll).take(editor_area.height as usize).map(|r| Line::from(inline_spans(&c.editor.row_text(r)))).collect()
    };
    f.render_widget(Paragraph::new(lines), editor_area);
    let cursor = Position::new(editor_area.x + cx as u16, editor_area.y + (cy - c.editor.scroll) as u16);
    if !c.submitting {
        f.set_cursor_position(cursor);
    }
    app.hits.editor = Some(editor_area);
    app.hits.editor_rows = rows;

    f.render_widget(Paragraph::new("─".repeat(sep.width as usize)).style(Style::new().fg(DIM)), sep);
    let [hint, btns] = Layout::horizontal([Constraint::Min(1), Constraint::Length(30)]).areas(bottom);
    let hint_text = if c.submitting {
        format!(" {} Sending…", SPINNER[app.tick % SPINNER.len()])
    } else {
        " Enter newline · Ctrl+S / Alt+Enter send · Esc close (draft kept)".into()
    };
    f.render_widget(Paragraph::new(Line::styled(hint_text, Style::new().fg(DIM))), hint);
    let send = "  Send ⌃S  ";
    let cancel = "  Cancel  ";
    let sr = Rect::new(btns.x + 2, btns.y, send.width() as u16, 1);
    let cr = Rect::new(sr.right() + 2, btns.y, cancel.width() as u16, 1);
    f.render_widget(Paragraph::new(send).style(Style::new().fg(Color::Black).bg(color).bold()), sr);
    f.render_widget(Paragraph::new(cancel).style(Style::new().fg(Color::Black).bg(Color::Gray)), cr);
    app.hits.buttons.push((sr, Action::Submit));
    app.hits.buttons.push((cr, Action::ModalCancel));

    // @mention popup anchored under the cursor.
    if !candidates.is_empty() && !c.submitting {
        let sel = c.popup_sel.min(candidates.len() - 1);
        let w = candidates.iter().map(|s| s.width()).max().unwrap_or(10).max(16) as u16 + 8;
        let h = candidates.len() as u16 + 2;
        let mut x = cursor.x.saturating_sub(1);
        if x + w > area.right() {
            x = area.right().saturating_sub(w);
        }
        let y = if cursor.y + 1 + h <= fa.bottom() { cursor.y + 1 } else { cursor.y.saturating_sub(h) };
        let popup = Rect::new(x, y, w, h).intersection(fa);
        f.render_widget(Clear, popup);
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(Color::Yellow)).title(" mention ");
        let pi = block.inner(popup);
        f.render_widget(block, popup);
        let lines: Vec<Line> = candidates
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let is_bot = app.cfg.bots.iter().any(|b| b.eq_ignore_ascii_case(name));
                let line = Line::from(vec![
                    Span::raw(" @"),
                    Span::styled(name.clone(), Style::new().bold()),
                    Span::styled(if is_bot { " bot" } else { "" }, Style::new().fg(DIM)),
                ]);
                if i == sel { line.style(Style::new().bg(Color::Indexed(238)).fg(Color::Yellow)) } else { line }
            })
            .collect();
        for i in 0..candidates.len() as u16 {
            if i < pi.height {
                app.hits.popup_items.push((Rect::new(pi.x, pi.y + i, pi.width, 1), i as usize));
            }
        }
        f.render_widget(Paragraph::new(lines), pi);
    }
}

/// One image fitted to (almost) the whole screen. Drawn after `dim()`, so its cells stay intact.
fn draw_image_modal(f: &mut Frame, app: &mut App) {
    let Some(Modal::Image { key, alt }) = &app.modal else { return };
    let (key, alt) = (key.clone(), alt.clone());
    let fa = f.area();
    let area = centered(fa, fa.width.saturating_sub(4), fa.height.saturating_sub(4));
    f.render_widget(Clear, area);
    let block = modal_block(&truncate(&alt, (area.width as usize).saturating_sub(6)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hits.modal = Some(area);
    let [pic, hint] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let message = |f: &mut Frame, text: String, color: Color| {
        let r = Rect::new(pic.x, pic.y + pic.height / 2, pic.width, 1);
        f.render_widget(Paragraph::new(Line::styled(text, Style::new().fg(color))).centered(), r);
    };
    let store = &mut app.images;
    match store.look(&key) {
        Look::Ready(dims) => {
            let size = images::fit(dims, store.font(), pic.width, pic.height);
            let (x, y) = ((pic.width - size.width) / 2, (pic.height - size.height) / 2);
            match store.proto(&key, size) {
                Some(proto) => f.render_widget(SlicedImage::new(proto, SignedPosition { x: x as i16, y: y as i16 }), pic),
                None => image_box(f.buffer_mut(), pic, Rect::new(x, 0, size.width, size.height), y as isize, "rendering…"),
            }
        }
        Look::Pending => {
            store.want_now(&key, (pic.width, pic.height));
            message(f, format!("{} Loading image…", SPINNER[app.tick % SPINNER.len()]), ACCENT);
        }
        Look::Failed(e) => message(f, format!("Couldn't load the image: {e} — o opens it in the browser"), Color::Red),
        Look::Skipped(why) => message(f, format!("Not shown inline: {} — o opens it in the browser", why.label()), DIM),
    }
    store.pump(&app.client, &app.tx);
    f.render_widget(Paragraph::new(Line::styled(" o / Enter open in browser · y copy URL · Esc close", Style::new().fg(DIM))), hint);
}

fn draw_help(f: &mut Frame, app: &mut App) {
    let rows: &[(&str, &str)] = &[
        ("", "Everywhere"),
        ("s / click scope", "Choose the scope (account, org, review queue…)"),
        ("S / click repo chip", "Filter to one repository (type to search)"),
        ("e", "Toggle the filter to the selected PR's repository"),
        ("f / click state", "Cycle Open → Closed → Merged → All"),
        ("r  F5", "Refresh now (auto-refreshes in the background)"),
        ("/  click ⌕ Search", "Search PRs (#, title, description…), repos, commands"),
        ("  Enter ^F ^G Tab Esc", "Open + filter · filter list · GitHub · complete · close"),
        ("o", "Open the selected PR in your browser"),
        ("y", "Copy `gh pr checkout` command to clipboard"),
        ("D / click ≡", "Display density: Comfortable → Cozy → Compact"),
        ("u / click N unread", "Only PRs with changes since you last opened them (• in the gutter)"),
        ("U", "Mark every listed PR read (opening a PR marks it read; J/K after 1 s)"),
        ("/ Sort unread first", "Unread PRs at the top (palette command)"),
        ("?", "This help · Ctrl+C quits"),
        ("", "List"),
        ("↑↓ j k  PgUp PgDn  g G", "Move selection"),
        ("Esc", "Clear the unread filter, then the search filter"),
        ("Enter / double-click", "Open the PR detail view"),
        ("", "Detail view"),
        ("Tab  h l  ← →  click", "Switch focus between list and details"),
        ("j k / wheel", "Scroll details (or move in the list when it has focus)"),
        ("J K", "Next / previous PR while reading details"),
        ("1-8  [ ]  click tab", "Description · Conversation · Changes · Commits · Checks · Actions · Reviews · Stack"),
        ("n p  ↑ ↓  click", "Pick a file in Changes (j k scroll its diff)"),
        ("n p  click +n / ── new", "Next / previous NEW entry (Conversation, Commits, Reviews)"),
        ("i", "Collapse / expand the PR info header"),
        ("I / click img:", "Show / hide inline images (click one to enlarge, o opens it)"),
        ("c", "Comment (type @ for mention autocomplete)"),
        ("m", "Merge menu (methods allowed by the repo)"),
        ("a  /  R", "Approve  /  Request changes"),
        ("x", "Close or reopen the PR"),
        ("Space  .", "All actions"),
        ("Esc  q", "Back to the full list"),
        ("", "Actions tab"),
        ("n p  ↑ ↓  click", "Move in the runs/jobs tree (j k scroll the log)"),
        ("Enter / click", "Fold a run's jobs · show a job's log"),
        ("O", "Open the selected run or job in your browser"),
        ("H", "Also show runs from earlier pushes"),
        ("E  W", "Next error / warning in the log"),
        ("z  Z  click ▸", "Fold the group at the cursor / all groups"),
        ("t", "Show / hide log timestamps"),
        ("Ctrl+F  n N", "Search the log · next / previous match"),
        ("Esc", "Clear search, then back from a job to its run"),
        ("", "Stacks (⧉ 2/4 = position / size; accent = GitHub stack, gray = branch chain)"),
        ("< >  click #n in the strip", "Previous (toward the base) / next PR in the stack"),
        ("8 · n p ↑ ↓ · Enter · Esc", "Stack tab: pick a member or Whole stack · its files, a file's diff · back"),
        ("b", "Update the PR's branch onto its parent (rebase, else merge; asks first)"),
        ("L", "Link a branch chain into a GitHub stack (asks first)"),
        ("M", "Merge a GitHub stack up to here: the PRs below it merge too (asks first)"),
        ("/ Group by stack", "Keep a stack's PRs together in the list, ├ └ (palette, on by default)"),
        ("", "Mouse"),
        ("click", "Select rows, buttons, menu items, check links"),
        ("click ↻ digest", "Jump to a PR the last refresh changed (status line)"),
        ("click outside", "Dismiss a popup"),
        ("", "config.toml"),
        ("images = …", "auto · kitty · sixel · iterm2 · halfblocks · off  (GH_PR_MANAGER_IMAGES)"),
        ("[notify] bell desktop", "Bell / desktop notice: review asked, verdict, CI failed (off)"),
    ];
    let area = centered(f.area(), 88, rows.len() as u16 + 4);
    f.render_widget(Clear, area);
    let block = modal_block("Help");
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hits.modal = Some(area);
    let mut lines: Vec<Line> = rows
        .iter()
        .map(|(k, d)| {
            if k.is_empty() {
                Line::styled(format!(" {d}"), Style::new().fg(ACCENT).bold())
            } else {
                Line::from(vec![Span::styled(format!("   {k:<24}"), Style::new().fg(Color::Yellow)), Span::raw(d.to_string())])
            }
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(" press any key to close", Style::new().fg(DIM)));
    f.render_widget(Paragraph::new(lines), inner);
}

// ---------- search ----------

/// `text` cut to `max` columns, the chars at `idx` (sorted char indices) in the accent colour.
fn hl_spans(text: &str, idx: &[u32], base: impl Fn(usize) -> Style, max: usize) -> Vec<Span<'static>> {
    let cut = truncate(text, max);
    let n = cut.chars().count();
    let ellipsis = cut != text;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for (i, c) in cut.chars().enumerate() {
        let hit = !(ellipsis && i + 1 == n) && idx.binary_search(&(i as u32)).is_ok();
        let st = if hit { base(i).fg(ACCENT).bold() } else { base(i) };
        if cur.is_some_and(|s| s != st) {
            spans.push(Span::styled(std::mem::take(&mut buf), cur.unwrap_or_default()));
        }
        cur = Some(st);
        buf.push(c);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, cur.unwrap_or_default()));
    }
    spans
}

fn ranges_idx(ranges: &[(usize, usize)]) -> Vec<u32> {
    let mut v: Vec<u32> = ranges.iter().flat_map(|&(a, b)| a as u32..b as u32).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// `#123`, dim, with the typed digits highlighted.
fn number_spans(number: u64, idx: &[u32]) -> Vec<Span<'static>> {
    let shifted: Vec<u32> = idx.iter().map(|i| i + 1).collect();
    hl_spans(&format!("#{number}"), &shifted, |_| Style::new().fg(DIM), usize::MAX)
}

/// Only the description matched (nothing to highlight in the row itself).
fn body_only(m: &Marks) -> bool {
    m.title.is_empty() && m.repo.is_empty() && m.number.is_empty() && m.author.is_empty() && m.snippet.is_some()
}

/// The palette input, each token coloured: filters accent, invalid ones red.
fn search_input_spans(app: &App) -> Vec<Span<'static>> {
    use crate::search::Token;
    let q = app.search.query();
    let raw = &app.search.text;
    let mut spans = Vec::new();
    if q.number_mode && !raw.trim_start().starts_with('#') {
        spans.push(Span::styled("#", Style::new().fg(DIM)));
    }
    let mut at = 0;
    for (t, r) in q.tokens.iter().zip(&q.spans) {
        if r.start > at {
            spans.push(Span::raw(raw[at..r.start].to_string()));
        }
        let style = match t {
            Token::Invalid => Style::new().fg(Color::Red),
            Token::Pending => Style::new().fg(ACCENT),
            Token::Number(..) => Style::new().fg(Color::Yellow).bold(),
            t if t.is_filter() => Style::new().fg(ACCENT).bold(),
            _ => Style::new(),
        };
        spans.push(Span::styled(raw[r.clone()].to_string(), style));
        at = r.end;
    }
    if at < raw.len() {
        spans.push(Span::raw(raw[at..].to_string()));
    }
    spans
}

/// One PR row: marker, state (or `↗`), repo, #number, title, author · age on the right.
#[allow(clippy::too_many_arguments)]
fn search_pr_line(p: &PrSummary, mk: &Marks, lead: Span<'static>, chip: Option<Span<'static>>, repo_w: usize, w: usize, selected: bool, remote: bool) -> Line<'static> {
    let mut spans = vec![Span::styled(if selected { "▌" } else { " " }, Style::new().fg(ACCENT)), lead, Span::raw(" ")];
    let nwo = &p.repository.name_with_owner;
    let owner_len = nwo.find('/').map_or(0, |i| nwo[..i].chars().count() + 1);
    let repo = hl_spans(nwo, &mk.repo, |i| if i < owner_len { Style::new().fg(DIM) } else { Style::new().bold() }, repo_w);
    let repo_used: usize = repo.iter().map(|s| s.width()).sum();
    spans.extend(repo);
    spans.push(Span::raw(" ".repeat(repo_w.saturating_sub(repo_used) + 2)));
    let num = number_spans(p.number, &mk.number);
    let num_used: usize = num.iter().map(|s| s.width()).sum();
    spans.extend(num);
    spans.push(Span::raw(" ".repeat(7usize.saturating_sub(num_used))));
    if let Some(c) = chip {
        spans.push(c);
        spans.push(Span::raw(" "));
    }
    let mut right = format!("{} · {}", login_of(&p.author), ago(&p.updated_at));
    let used: usize = spans.iter().map(|s| s.width()).sum();
    let mut title_w = w.saturating_sub(used + right.width() + 2);
    if title_w < 16 {
        right.clear();
        title_w = w.saturating_sub(used + 1);
    }
    let state = match (p.state.as_str(), p.is_draft) {
        _ if !remote => "",
        ("MERGED", _) => "  (merged)",
        ("CLOSED", _) => "  (closed)",
        (_, true) => "  (draft)",
        _ => "",
    };
    let mut title = hl_spans(&p.title, &mk.title, |_| if p.is_draft { Style::new().fg(Color::Gray) } else { Style::new() }, title_w.saturating_sub(state.width()));
    title.push(Span::styled(state, Style::new().fg(DIM)));
    let title_used: usize = title.iter().map(|s| s.width()).sum();
    spans.extend(title);
    spans.push(Span::raw(" ".repeat(title_w.saturating_sub(title_used) + 1)));
    spans.push(Span::styled(right, Style::new().fg(DIM)));
    Line::from(spans)
}

/// Text plus a right-aligned dim note, padded to `w`.
fn with_right(mut spans: Vec<Span<'static>>, right: String, w: usize) -> Line<'static> {
    let used: usize = spans.iter().map(|s| s.width()).sum();
    if used + right.width() + 1 <= w {
        spans.push(Span::raw(" ".repeat(w - used - right.width())));
        spans.push(Span::styled(right, Style::new().fg(DIM)));
    }
    Line::from(spans)
}

fn draw_search(f: &mut Frame, app: &mut App) {
    let items = search_items(app);
    let q = app.search.query();
    let density = app.state.density;
    let fa = f.area();
    let w = 96.min(fa.width.saturating_sub(2));
    let lw = w.saturating_sub(2) as usize;
    let snippets = lw >= 60 && density != Density::Compact;
    let spin = SPINNER[app.tick % SPINNER.len()];
    let paused = app.search.paused_for();
    let remote = app.current_remote(&q);
    let scope_desc = {
        let mut d = app.scopes[app.scope].name.clone();
        if let Some(r) = &app.repo {
            d = format!("{r} ({d})");
        }
        format!("{d} · {}", app.state_filter.label())
    };
    let typing = app.search.text.trim().to_string();

    // Visual rows: (line, selectable index, clicking it selects).
    let repo_w = items
        .iter()
        .filter_map(|it| match it {
            Item::Pr { idx, .. } => Some(app.prs[*idx].repository.name_with_owner.width()),
            Item::Remote { i, .. } => remote.and_then(|r| r.hits.get(*i)).map(|p| p.repository.name_with_owner.width()),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .min(lw / 3);
    let remote_rows = items.iter().filter(|it| matches!(it, Item::Remote { .. })).count();
    // (line, its selectable index, or for a header the index its click highlights)
    let mut rows: Vec<(Line<'static>, Option<usize>, Option<usize>)> = Vec::new();
    let mut k = 0usize;
    let mut first_section = true;
    for it in &items {
        let selectable = it.selectable();
        let selected = selectable && k == app.search.sel;
        let idx = selectable.then_some(k);
        let mark = || Span::styled(if selected { "▌" } else { " " }, Style::new().fg(ACCENT));
        match it {
            Item::Header(t) => {
                if density == Density::Comfortable && !first_section {
                    rows.push((Line::raw(""), None, None));
                }
                first_section = false;
                let mut spans = vec![Span::styled(format!(" {t}"), Style::new().fg(ACCENT).bold())];
                if t == "From GitHub" {
                    match (remote, q.number()) {
                        (Some(r), Some(n)) if r.loading => {
                            spans.push(Span::styled(format!(" · {spin} looking up #{n} in {} repositories…", r.repos), Style::new().fg(ACCENT)));
                        }
                        (Some(r), _) if r.error.is_some() => {
                            spans.push(Span::styled(format!(" · failed: {}", r.error.clone().unwrap_or_default()), Style::new().fg(Color::Red)));
                        }
                        (_, Some(_)) => spans.push(Span::styled(format!(" · {remote_rows} found"), Style::new().fg(DIM))),
                        _ => spans.push(Span::styled(format!(" · {remote_rows} more"), Style::new().fg(DIM))),
                    }
                }
                rows.push((Line::from(spans), None, Some(k)));
            }
            Item::Note(t) => rows.push((Line::styled(format!("   {t}"), Style::new().fg(DIM).italic()), None, None)),
            Item::Pr { idx: i, m } => {
                let p = &app.prs[*i];
                let mk = app.marks(&q, *i, (snippets && m.body_hit.is_some()).then_some(lw.saturating_sub(8)));
                let chip = chip_span(app.chip(&p.id).or_else(|| summary_chip(p)));
                rows.push((search_pr_line(p, &mk, state_icon(&p.state, p.is_draft), chip, repo_w, lw, selected, false), idx, None));
                if let Some((text, ranges)) = &mk.snippet {
                    let mut spans = vec![Span::raw("     ")];
                    spans.extend(hl_spans(text, &ranges_idx(ranges), |_| Style::new().fg(DIM), lw.saturating_sub(6)));
                    rows.push((Line::from(spans), idx, None));
                }
            }
            Item::Remote { i, m } => {
                let Some(p) = remote.and_then(|r| r.hits.get(*i)) else { continue };
                let mk = app.remote_marks(&q, *i, (snippets && m.body_hit.is_some()).then_some(lw.saturating_sub(8)));
                rows.push((search_pr_line(p, &mk, Span::styled("↗", Style::new().fg(DIM)), chip_span(summary_chip(p)), repo_w, lw, selected, true), idx, None));
                if let Some((text, ranges)) = &mk.snippet {
                    let mut spans = vec![Span::raw("     ")];
                    spans.extend(hl_spans(text, &ranges_idx(ranges), |_| Style::new().fg(DIM), lw.saturating_sub(6)));
                    rows.push((Line::from(spans), idx, None));
                }
            }
            Item::More(n) => {
                let spans = vec![mark(), Span::styled(format!("  … {n} more loaded matches"), Style::new().fg(DIM).italic())];
                rows.push((with_right(spans, "Enter / ^F filter the list ".into(), lw), idx, None));
            }
            Item::Repo { nwo, count } => {
                let hl = crate::search::text_indices(&mut crate::search::matcher(), &q, nwo);
                let owner_len = nwo.find('/').map_or(0, |i| nwo[..i].chars().count() + 1);
                let mut spans = vec![mark(), Span::styled("◫ ", Style::new().fg(Color::Blue))];
                spans.extend(hl_spans(nwo, &hl, |i| if i < owner_len { Style::new().fg(DIM) } else { Style::new().bold() }, lw.saturating_sub(24)));
                let right = match count {
                    Some(c) => format!("{c} PRs · filter "),
                    None => "filter ".into(),
                };
                rows.push((with_right(spans, right, lw), idx, None));
            }
            Item::Scope(i) => {
                let sc = &app.scopes[*i];
                let hl = crate::search::text_indices(&mut crate::search::matcher(), &q, &sc.name);
                let mut spans = vec![mark(), Span::styled(if *i == app.scope { "✓ " } else { "◉ " }, Style::new().fg(Color::Green))];
                spans.extend(hl_spans(&sc.name, &hl, |_| Style::new().bold(), lw / 2));
                spans.push(Span::styled(format!("  {}", truncate(&sc.qualifier, lw / 3)), Style::new().fg(DIM)));
                rows.push((with_right(spans, "scope ".into(), lw), idx, None));
            }
            Item::Command { cmd, disabled } => {
                let label = match (cmd, app.selected_detail()) {
                    (Command::CloseOrReopen, Some(d)) if d.state == "CLOSED" => "Reopen pull request",
                    (Command::CloseOrReopen, _) => "Close pull request",
                    _ => cmd.label(),
                };
                let style = if disabled.is_some() { Style::new().fg(DIM) } else { Style::new().bold() };
                let hl = crate::search::text_indices(&mut crate::search::matcher(), &q, cmd.label());
                let hl = if label == cmd.label() { hl } else { Vec::new() };
                let mut spans = vec![mark(), Span::styled("▸ ", Style::new().fg(ACCENT))];
                spans.extend(hl_spans(label, &hl, |_| style, lw / 2));
                if cmd.needs_pr()
                    && let Some(p) = app.selected_pr()
                {
                    spans.push(Span::styled(format!("  #{}", p.number), Style::new().fg(DIM)));
                }
                if let Some(why) = disabled {
                    spans.push(Span::styled(format!(" — {why}"), Style::new().fg(DIM).italic()));
                }
                let key = if cmd.key().is_empty() { String::new() } else { format!("{} ", cmd.key()) };
                rows.push((with_right(spans, key, lw), idx, None));
            }
            Item::Recent(r) => {
                let spans = vec![mark(), Span::styled("↺ ", Style::new().fg(DIM)), Span::raw(r.clone())];
                rows.push((Line::from(spans), idx, None));
            }
            Item::SearchGitHub => {
                let quoted = format!("“{}”", truncate(&typing, 32));
                let incomplete = q.complete < q.tokens.len()
                    && app.search.text.trim_end() == app.search.text
                    && matches!(q.tokens.last(), Some(crate::search::Token::Term(..)));
                let (text, style, right) = if let Some(why) = &app.search.blocked {
                    (format!("↗ GitHub search off: {why}"), Style::new().fg(Color::Red), String::new())
                } else if remote.is_some_and(|r| r.loading) {
                    (format!("{spin} searching GitHub for {quoted} in {scope_desc}…"), Style::new().fg(ACCENT), String::new())
                } else if let Some(e) = remote.and_then(|r| r.error.clone()) {
                    (format!("↗ GitHub search failed: {e}"), Style::new().fg(Color::Red), "retry ^G ".into())
                } else if let (Some(secs), true) = (paused, app.search.text.trim().chars().count() >= 3 && remote.is_none()) {
                    (format!("⏳ GitHub search paused ({secs} s)"), Style::new().fg(Color::Yellow), "^G ".into())
                } else if remote.is_some() && remote_rows == 0 {
                    (format!("↗ Nothing more on GitHub for {quoted} in {scope_desc}"), Style::new().fg(DIM), "again ^G ".into())
                } else if remote.is_some() {
                    (format!("↗ Searched GitHub for {quoted} in {scope_desc}"), Style::new().fg(DIM), "again ^G ".into())
                } else if incomplete && app.cfg.search.live && app.search.text.trim().chars().count() >= 3 {
                    (format!("↗ Search GitHub for {quoted} — it matches whole words; pause, finish the word or ^G"), Style::new().fg(DIM), String::new())
                } else {
                    (format!("↗ Search GitHub for {quoted} in {scope_desc}"), Style::new(), "^G ".into())
                };
                let spans = vec![mark(), Span::styled(truncate(&text, lw.saturating_sub(right.width() + 3)), style)];
                rows.push((with_right(spans, right, lw), idx, None));
            }
        }
        if selectable {
            k += 1;
        }
    }

    // Box: anchored near the top so it grows downwards as results come in.
    let rules = density.rules();
    let chrome = 2 + 1 + 2 + if rules { 2 } else { 0 };
    let max_h = (fa.height * 4 / 5).max(chrome + 3);
    let h = (rows.len() as u16 + chrome).clamp(chrome + 3, max_h).min(fa.height);
    let top = fa.y + (fa.height.saturating_sub(h) / 6).min(fa.height / 8).max(fa.height.saturating_sub(h).min(1));
    let area = Rect::new(fa.x + (fa.width - w) / 2, top, w, h);
    f.render_widget(Clear, area);
    let block = modal_block("Search");
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hits.modal = Some(area);
    let rule_h = if rules { 1 } else { 0 };
    let [input, sep1, list, sep2, keys, prefixes] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(rule_h),
        Constraint::Min(1),
        Constraint::Length(rule_h),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    // Input line with the count (or the number-mode hint) on the right.
    let mut spans = vec![Span::styled(" › ", Style::new().fg(ACCENT))];
    let typed = search_input_spans(app);
    let right = if q.number_mode {
        "type a PR number · Enter opens an exact match ".to_string()
    } else if q.is_empty() {
        String::new()
    } else {
        let mut r = format!("{} of {}", app.palette_results().len(), app.prs.len());
        if let Some((done, all)) = app.bodies_progress() {
            r.push_str(&format!(" · {spin} {done}/{all}"));
        } else if !app.cfg.search.bodies {
            r.push_str(" · descriptions off");
        }
        r + " "
    };
    let typed_w: usize = typed.iter().map(|s| s.width()).sum();
    let room = (input.width as usize).saturating_sub(4 + right.width());
    if q.is_empty() && app.search.text.is_empty() {
        spans.push(Span::styled("▏", Style::new().fg(ACCENT)));
        spans.push(Span::styled("PRs, #numbers, repos, scopes, commands…", Style::new().fg(DIM)));
    } else if typed_w > room {
        // Long input: show its tail.
        let text = app.search.text.clone();
        let tail: String = text.chars().rev().scan(0, |acc, c| {
            *acc += c.width().unwrap_or(0);
            (*acc < (input.width as usize).saturating_sub(5)).then_some(c)
        }).collect::<Vec<_>>().into_iter().rev().collect();
        spans.push(Span::raw(format!("…{tail}")));
        spans.push(Span::styled("▏", Style::new().fg(ACCENT)));
    } else {
        spans.extend(typed);
        spans.push(Span::styled("▏", Style::new().fg(ACCENT)));
    }
    let used: usize = spans.iter().map(|s| s.width()).sum();
    if used + right.width() <= input.width as usize {
        spans.push(Span::raw(" ".repeat(input.width as usize - used - right.width())));
        spans.push(Span::styled(right, Style::new().fg(DIM)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), input);
    if rules {
        f.render_widget(Paragraph::new("─".repeat(sep1.width as usize)).style(Style::new().fg(DIM)), sep1);
        f.render_widget(Paragraph::new("─".repeat(sep2.width as usize)).style(Style::new().fg(DIM)), sep2);
    }

    // Scroll so the highlighted row (both lines of it) is in view, keeping its section header when possible.
    let hgt = list.height as usize;
    let sel_first = rows.iter().position(|(_, i, _)| *i == Some(app.search.sel)).unwrap_or(0);
    let sel_last = rows.iter().rposition(|(_, i, _)| *i == Some(app.search.sel)).unwrap_or(0);
    let offset = if sel_last >= hgt { sel_last + 1 - hgt } else { 0 };
    let offset = offset.min(sel_first.saturating_sub(1).max(sel_first.min(offset)));
    let mut lines = Vec::new();
    for (row, (line, idx, target)) in rows.into_iter().enumerate().skip(offset).take(hgt) {
        let y = list.y + (row - offset) as u16;
        let r = Rect::new(list.x, y, list.width, 1);
        match idx {
            Some(i) => {
                app.hits.modal_items.push((r, i));
                lines.push(if i == app.search.sel { line.style(Style::new().bg(Color::Indexed(238))) } else { line });
            }
            None => {
                // A section header: clicking it highlights its first row.
                if let Some(t) = target.filter(|t| *t < k) {
                    app.hits.buttons.push((r, Action::SearchSelect(t)));
                }
                lines.push(line);
            }
        }
    }
    f.render_widget(Paragraph::new(lines), list);
    let hint = " ↑↓ move · Enter open · ^F filter list · Tab complete · ^G GitHub · Esc close";
    f.render_widget(Paragraph::new(Line::styled(truncate(hint, keys.width as usize), Style::new().fg(DIM))), keys);

    // Clickable filter hints.
    let mut x = prefixes.x + 1;
    let mut spans = vec![Span::raw(" ")];
    for (label, token) in [("repo:", "repo:"), ("author:", "author:"), ("label:", "label:"), ("head:", "head:"), ("#123", "#"), ("is:draft", "is:draft"), ("ci:failing", "ci:failing"), ("review:approved", "review:approved")] {
        let lw = label.width() as u16;
        if x + lw > prefixes.right() {
            break;
        }
        app.hits.buttons.push((Rect::new(x, prefixes.y, lw, 1), Action::SearchInsert(token.to_string())));
        spans.push(Span::styled(label, Style::new().fg(DIM).add_modifier(Modifier::UNDERLINED)));
        spans.push(Span::raw("  "));
        x += lw + 2;
    }
    f.render_widget(Paragraph::new(Line::from(spans)), prefixes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ts("2026-10-17T13:18:32Z"), Some(1_792_243_112));
    }

    #[test]
    fn strips_html_comments() {
        assert_eq!(clean_body("a<!-- x -->b\r\n\n\n\nc"), "ab\n\nc");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(fmt_duration(0), "0s");
        assert_eq!(fmt_duration(59), "59s");
        assert_eq!(fmt_duration(61), "1m01s");
        assert_eq!(fmt_duration(3661), "1h01m");
    }

    #[test]
    fn maps_rest_outcomes() {
        assert_eq!(rest_outcome("completed", Some("action_required")), "ACTION_REQUIRED");
        assert_eq!(rest_outcome("in_progress", None), "IN_PROGRESS");
    }

    const SHOT: &str = "https://private-user-images.githubusercontent.com/1/2-0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0.png?jwt=x";
    const KEY: &str = "https://github.com/user-attachments/assets/0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0";

    fn store() -> ImageStore {
        ImageStore::new(Some(ratatui_image::picker::Picker::halfblocks()), true, String::new())
    }

    fn build(html: &str, store: &ImageStore) -> (Vec<DetailRow>, Vec<ImagePlacement>) {
        let mut rows = vec![(Line::raw("above"), Vec::new())];
        let mut img = Imgs::new(store);
        assert!(html_lines(html, 80, "", &mut rows, &mut img));
        (rows, img.out)
    }

    fn lone(src: &str) -> String {
        format!(r#"<p>Before</p><p><a href="{src}"><img src="{src}" alt="Screenshot" style="max-width: 100%;"></a></p><p>After</p>"#)
    }

    #[test]
    fn lone_image_gets_a_placeholder_then_rows() {
        let mut store = store();
        let (rows, placements) = build(&lone(SHOT), &store);
        assert_eq!(placements.len(), 1, "{rows:?}");
        let p = &placements[0];
        assert_eq!((p.key.as_str(), p.rows, p.alt.as_str()), (KEY, 1, "Screenshot"));
        assert!(rows[p.row].0.to_string().contains("▣ Screenshot (loading…)"));
        assert_eq!(rows[p.row].1, vec![(0, u16::MAX, format!("#image:{KEY}"))]);

        // Once the size is known the image reserves its rows: 400×200 px at 10×20 px cells.
        let image = std::sync::Arc::new(image::DynamicImage::new_rgba8(400, 200));
        store.apply(images::ImageMsg::Loaded { key: KEY.into(), res: Ok(images::Decoded { dims: (400, 200), image, first: None }) });
        let (rows, placements) = build(&lone(SHOT), &store);
        let p = &placements[0];
        assert_eq!((p.rows, p.cols), (10, 40));
        for r in &rows[p.row..p.row + 10] {
            assert!(r.0.to_string().trim().is_empty());
            assert_eq!(r.1, vec![(0, u16::MAX, format!("#image:{KEY}"))]);
        }
        assert!(rows[p.row + 10..].iter().any(|r| r.0.to_string().contains("After")));
    }

    #[test]
    fn failed_image_keeps_one_row() {
        let mut store = store();
        store.apply(images::ImageMsg::Loaded { key: KEY.into(), res: Err(images::Fail::Failed("HTTP 404".into())) });
        let (rows, placements) = build(&lone(SHOT), &store);
        assert!(placements.is_empty());
        let row = rows.iter().find(|r| r.0.to_string().contains("Screenshot")).unwrap();
        assert!(row.0.to_string().contains("(failed: HTTP 404)"));
        assert_eq!(row.1, vec![(0, u16::MAX, KEY.to_string())]);
    }

    #[test]
    fn badges_and_inline_images_stay_labels() {
        let store = store();
        for html in [
            format!(r#"<p><img src="{SHOT}" alt="badge" width="16"></p>"#),
            format!(r#"<p><a href="https://vercel.com"><img src="{SHOT}" alt="Preview"></a> Ready to deploy</p>"#),
            format!(r#"<table><tr><td><img src="{SHOT}" alt="before"></td><td>after</td></tr></table>"#),
            r#"<p><img src="https://camo.githubusercontent.com/a/b" data-canonical-src="https://x.io/badge.svg" alt="ok"></p>"#.to_string(),
        ] {
            let (rows, placements) = build(&html, &store);
            assert!(placements.is_empty(), "{html}");
            assert!(rows.iter().any(|r| r.0.to_string().contains('▣')), "{html}: {rows:?}");
        }
        // Images toggled off: the lone screenshot is a label as well.
        let mut off = store;
        off.enabled = false;
        assert!(build(&lone(SHOT), &off).1.is_empty());
    }

    #[test]
    fn sanitize_skips_image_cells() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 3, 1));
        buf[(0, 0)].set_symbol("\u{1b}Pq#0;2;0;0;0\u{1b}\\");
        buf[(1, 0)].set_symbol("\u{10EEEE}\u{0305}");
        buf[(2, 0)].set_symbol("❤\u{FE0F}");
        sanitize(&mut buf);
        assert!(buf[(0, 0)].symbol().starts_with('\u{1b}'));
        assert_eq!(buf[(1, 0)].symbol(), "\u{10EEEE}\u{0305}");
        assert_eq!(buf[(2, 0)].symbol(), "❤");
    }

    #[test]
    fn wraps_words() {
        let out = wrap(Line::raw("hello world foo"), 11, "  ");
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].to_string(), "  foo");
    }
}
