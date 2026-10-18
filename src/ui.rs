//! Rendering. Every clickable region is recorded into `app.hits` while drawing.

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{Action, App, DetailRow, ComposeKind, Focus, Hits, Modal, Screen, ToastKind, scope_matches};
use crate::github::{PrDetail, PrSummary, login_of};

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub fn draw(f: &mut Frame, app: &mut App) {
    app.hits = Hits::default();
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

    match &app.modal {
        Some(Modal::Scopes { .. }) => draw_scopes(f, app),
        Some(Modal::Menu { .. }) => draw_menu(f, app),
        Some(Modal::Confirm { .. }) => draw_confirm(f, app),
        Some(Modal::Composer(_)) => draw_composer(f, app),
        Some(Modal::Help) => draw_help(f, app),
        None => {}
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
fn parse_ts(ts: &str) -> Option<i64> {
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
        0..60 => "now".into(),
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

// ---------- header / status / footer ----------

fn draw_header(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_widget(Block::new().style(Style::new().bg(Color::Indexed(236))), area);
    let scope = app.scopes[app.scope].name.clone();
    let mut x = area.x;
    let mut spans = vec![Span::styled(" ⎇ PR Manager ", Style::new().fg(Color::Black).bg(ACCENT).bold()), Span::raw(" ")];
    x += 15;
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
    spans.push(Span::raw("  "));

    let shown = app.visible().len();
    let count = if !app.filter.is_empty() {
        format!("{shown} matching of {}", app.prs.len())
    } else if app.total > app.prs.len() {
        format!("{} of {} PRs", app.prs.len(), app.total)
    } else {
        format!("{} PRs", app.prs.len())
    };
    spans.push(Span::styled(count, Style::new().fg(Color::Gray)));
    if app.loading_list || !app.detail_loading.is_empty() || !app.busy.is_empty() {
        spans.push(Span::styled(format!("  {} loading", SPINNER[app.tick % SPINNER.len()]), Style::new().fg(ACCENT)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);

    let mut right = Vec::new();
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
        app.hits.buttons.push((Rect::new(area.right() - 3, area.y, 3, 1), Action::Help));
        f.render_widget(Paragraph::new(Line::from(right)), r);
    }
}

fn draw_status(f: &mut Frame, app: &mut App, area: Rect) {
    let line = if let Some((msg, kind, _)) = &app.toast {
        let (icon, color) = match kind {
            ToastKind::Info => ("ℹ", ACCENT),
            ToastKind::Success => ("✓", Color::Green),
            ToastKind::Error => ("✗", Color::Red),
        };
        Line::from(vec![Span::styled(format!(" {icon} "), Style::new().fg(color).bold()), Span::styled(msg.clone(), Style::new().fg(color))])
    } else if app.filter_input || !app.filter.is_empty() {
        let mut spans = vec![Span::styled(" / ", Style::new().fg(ACCENT).bold()), Span::raw(app.filter.clone())];
        if app.filter_input {
            spans.push(Span::styled("▏", Style::new().fg(ACCENT)));
            spans.push(Span::styled("   Enter keep · Esc clear", Style::new().fg(DIM)));
        } else {
            spans.push(Span::styled("   (Esc to clear filter)", Style::new().fg(DIM)));
        }
        Line::from(spans)
    } else {
        Line::from(Span::styled(format!(" {}", app.search_query()), Style::new().fg(DIM)))
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
        (Some(_), _) => vec![("Enter", "Select", Action::ModalPick(usize::MAX)), ("Esc", "Close", Action::ModalCancel)],
        (None, Screen::List) => vec![
            ("↵", "Open", Action::OpenSelected),
            ("s", "Scope", Action::OpenScopes),
            ("f", "State", Action::CycleState),
            ("/", "Filter", Action::StartFilter),
            ("c", "Comment", Action::Comment),
            ("m", "Merge", Action::MergeMenu),
            ("o", "Browser", Action::Browser),
            ("r", "Refresh", Action::Refresh),
            ("?", "Help", Action::Help),
            ("q", "Quit", Action::Quit),
        ],
        (None, Screen::Detail) => vec![
            ("Esc", "Back", Action::Back),
            ("Tab", "Switch pane", Action::ToggleFocus),
            ("c", "Comment", Action::Comment),
            ("m", "Merge", Action::MergeMenu),
            ("a", "Approve", Action::Approve),
            ("x", "Close", Action::CloseOrReopen),
            ("␣", "All actions", Action::Menu),
            ("o", "Browser", Action::Browser),
            ("r", "Refresh", Action::Refresh),
            ("?", "Help", Action::Help),
        ],
    };
    // In modals the "Select" hint has no target index; drop it from the clickable set.
    let items: Vec<_> = items.into_iter().filter(|(_, _, a)| *a != Action::ModalPick(usize::MAX)).collect();
    button_row(f, &mut app.hits, area, &items, key);
}

// ---------- list screen ----------

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::new().borders(Borders::TOP).border_style(Style::new().fg(DIM));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let vis = app.visible();
    if vis.is_empty() {
        let msg = if let Some(e) = &app.list_error {
            vec![Line::styled(format!("Failed to load PRs: {e}"), Style::new().fg(Color::Red)), Line::raw(""), Line::raw("Press r to retry")]
        } else if app.loading_list {
            vec![Line::styled(format!("{} Loading pull requests…", SPINNER[app.tick % SPINNER.len()]), Style::new().fg(ACCENT))]
        } else if !app.filter.is_empty() {
            vec![Line::raw(format!("No PRs match “{}”", app.filter)), Line::styled("Esc clears the filter", Style::new().fg(DIM))]
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

    let rows: Vec<Row> = vis
        .iter()
        .map(|&i| {
            let p = &app.prs[i];
            Row::new(vec![
                Cell::from(Line::from(vec![Span::raw(" "), state_icon(&p.state, p.is_draft)])),
                Cell::from(Span::styled(p.repository.name_with_owner.clone(), Style::new().fg(Color::Blue))),
                Cell::from(Span::styled(format!("#{}", p.number), Style::new().fg(DIM))),
                Cell::from(Span::styled(p.title.clone(), if p.is_draft { Style::new().fg(Color::Gray) } else { Style::new() })),
                Cell::from(login_of(&p.author).to_string()),
                Cell::from(ci_icon(p.ci_state())),
                Cell::from(review_span(p.review_decision.as_deref())),
                Cell::from(Span::styled(
                    if p.comments.total_count > 0 { format!("💬{}", p.comments.total_count) } else { String::new() },
                    Style::new().fg(DIM),
                )),
                Cell::from(Span::styled(ago(&p.updated_at), Style::new().fg(DIM))),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(2),
        Constraint::Percentage(20),
        Constraint::Length(6),
        Constraint::Fill(1),
        Constraint::Length(14),
        Constraint::Length(2),
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(4),
    ];
    let header = Row::new(["", "Repository", "#", "Title", "Author", "CI", "Review", "", "Upd"])
        .style(Style::new().fg(Color::Gray).bold())
        .bottom_margin(0);
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
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
    let items: Vec<ListItem> = app
        .visible()
        .iter()
        .map(|&i| {
            let p = &app.prs[i];
            let num = format!("#{} ", p.number);
            let l1 = Line::from(vec![
                state_icon(&p.state, p.is_draft),
                Span::raw(" "),
                ci_icon(p.ci_state()),
                Span::raw(" "),
                Span::styled(num.clone(), Style::new().fg(DIM)),
                Span::raw(truncate(&p.title, w.saturating_sub(4 + num.width()))),
            ]);
            let meta = format!("    {} · {} · {}", p.repository.name_with_owner, login_of(&p.author), ago(&p.updated_at));
            let l2 = Line::styled(truncate(&meta, w), Style::new().fg(DIM));
            ListItem::new(vec![l1, l2])
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
    let [bar, sep, content] = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)]).areas(inner);

    // Clickable action bar.
    let fetched_at = app.details.get(&pr.id).map(|(d, at)| (d.state == "CLOSED", *at));
    let close_label = if fetched_at.is_some_and(|(closed, _)| closed) { "Reopen" } else { "Close" };
    let btn = Style::new().fg(Color::Black).bg(ACCENT);
    button_row(
        f,
        &mut app.hits,
        bar,
        &[
            ("💬", "Comment", Action::Comment),
            ("⇄", "Merge ▾", Action::MergeMenu),
            ("✓", "Approve", Action::Approve),
            ("✕", close_label, Action::CloseOrReopen),
            ("↗", "Browser", Action::Browser),
            ("⋯", "More", Action::Menu),
        ],
        btn,
    );
    f.render_widget(Paragraph::new("─".repeat(sep.width as usize)).style(Style::new().fg(DIM)), sep);

    let text_w = content.width.saturating_sub(2) as usize;
    let mut rows: Vec<DetailRow> = Vec::new();
    match fetched_at {
        Some((_, at)) => {
            let fresh = app.detail_cache.as_ref().is_some_and(|(id, cw, cat, _)| *id == pr.id && *cw == text_w && *cat == at);
            if !fresh {
                let mut built = Vec::new();
                detail_lines(&app.details[&pr.id].0, text_w, &mut built);
                app.detail_cache = Some((pr.id.clone(), text_w, at, built));
            }
            rows = app.detail_cache.as_ref().map(|c| c.3.clone()).unwrap_or_default();
        }
        None => {
            summary_lines(&pr, text_w, &mut rows);
            rows.push((Line::raw(""), Vec::new()));
            if let Some(e) = app.detail_errors.get(&pr.id) {
                rows.push((Line::styled(format!("Failed to load details: {e}"), Style::new().fg(Color::Red)), Vec::new()));
                rows.push((Line::styled("Press r to retry", Style::new().fg(DIM)), Vec::new()));
            } else {
                rows.push((Line::styled(format!("{} Loading details…", SPINNER[app.tick % SPINNER.len()]), Style::new().fg(ACCENT)), Vec::new()));
            }
        }
    }

    let height = content.height as usize;
    let max_scroll = rows.len().saturating_sub(height);
    app.detail_scroll = app.detail_scroll.min(max_scroll);
    app.hits.detail = Some(content);
    app.hits.detail_max_scroll = max_scroll;
    app.hits.detail_height = height;
    app.hits.detail_links = rows.iter().map(|(_, l)| l.clone()).collect();
    let visible: Vec<Line> = rows.into_iter().skip(app.detail_scroll).take(height).map(|(l, _)| l).collect();
    let text_area = Rect::new(content.x + 1, content.y, content.width.saturating_sub(2), content.height);
    app.hits.detail_text_x = text_area.x;
    f.render_widget(Paragraph::new(visible), text_area);
    if max_scroll > 0 {
        let mut sb = ScrollbarState::new(max_scroll).position(app.detail_scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None).thumb_style(Style::new().fg(ACCENT)),
            content,
            &mut sb,
        );
    }
}

fn push_wrapped(rows: &mut Vec<DetailRow>, line: Line<'static>, width: usize, indent: &str, link: Option<String>) {
    for l in wrap(line, width, indent) {
        rows.push((l, full_row_link(&link)));
    }
}

fn section(rows: &mut Vec<DetailRow>, title: &str, width: usize) {
    rows.push((Line::raw(""), Vec::new()));
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

fn detail_lines(d: &PrDetail, w: usize, rows: &mut Vec<DetailRow>) {
    let gray = Style::new().fg(Color::Gray);
    push_wrapped(rows, Line::styled(d.title.clone(), Style::new().bold()), w, "", None);
    let mut l = vec![state_badge(&d.state, d.is_draft), Span::raw(" ")];
    l.push(Span::styled(login_of(&d.author).to_string(), Style::new().fg(Color::Blue).bold()));
    l.push(Span::styled(format!(" opened {} ago · updated {} ago", ago(&d.created_at), ago(&d.updated_at)), gray));
    push_wrapped(rows, Line::from(l), w, "", None);
    push_wrapped(
        rows,
        Line::from(vec![
            Span::styled(d.base_ref_name.clone(), Style::new().fg(Color::Magenta)),
            Span::styled(" ← ", gray),
            Span::styled(d.head_ref_name.clone(), Style::new().fg(Color::Magenta)),
            Span::raw("   "),
            Span::styled(format!("+{}", d.additions), Style::new().fg(Color::Green)),
            Span::raw(" "),
            Span::styled(format!("−{}", d.deletions), Style::new().fg(Color::Red)),
            Span::styled(format!(" · {} files", d.changed_files), gray),
        ]),
        w,
        "",
        None,
    );
    rows.push((Line::raw(""), Vec::new()));

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

    // Mergeability.
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
    rows.push((Line::from(vec![field("Methods"), Span::styled(methods.join(" · "), gray)]), Vec::new()));
    if let Some(p) = &d.repository.viewer_permission {
        rows.push((Line::from(vec![field("Access"), Span::styled(p.to_lowercase(), gray)]), Vec::new()));
    }

    // Checks.
    if let Some(rollup) = d.rollup() {
        let mut checks: Vec<_> = rollup.contexts.iter().collect();
        let rank = |o: &str| match o {
            "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => 0,
            "PENDING" | "EXPECTED" => 1,
            "SUCCESS" => 2,
            _ => 3,
        };
        checks.sort_by_key(|c| (rank(&c.outcome()), c.label()));
        let (mut pass, mut fail, mut pend) = (0, 0, 0);
        for c in &checks {
            match rank(&c.outcome()) {
                0 => fail += 1,
                1 => pend += 1,
                2 => pass += 1,
                _ => {}
            }
        }
        section(rows, "Checks", w);
        rows.push((
            Line::from(vec![
                ci_icon(Some(&rollup.state)),
                Span::raw(" "),
                Span::styled(format!("{pass} passed"), Style::new().fg(Color::Green)),
                Span::raw(" · "),
                Span::styled(format!("{fail} failed"), Style::new().fg(if fail > 0 { Color::Red } else { DIM })),
                Span::raw(" · "),
                Span::styled(format!("{pend} pending"), Style::new().fg(if pend > 0 { Color::Yellow } else { DIM })),
                Span::styled("   (click a check to open it)", Style::new().fg(DIM)),
            ]),
            Vec::new(),
        ));
        for c in checks {
            let link = c.url().map(String::from);
            let label = truncate(&c.label(), w.saturating_sub(4));
            let style = if link.is_some() { Style::new().add_modifier(Modifier::UNDERLINED) } else { Style::new() };
            rows.push((Line::from(vec![Span::raw("  "), ci_icon(Some(&c.outcome())), Span::raw(" "), Span::styled(label, style)]), full_row_link(&link)));
        }
    }

    section(rows, "Description", w);
    if clean_body(&d.body).trim().is_empty() {
        rows.push((Line::styled("No description provided.", Style::new().fg(DIM).italic()), Vec::new()));
    } else {
        body_lines(&d.body, &d.body_html, w, "", rows);
    }

    // Timeline: comments, reviews and inline review comments, oldest first.
    enum Item<'a> {
        Comment(&'a crate::github::Comment),
        Review(&'a crate::github::Review),
        Inline(&'a crate::github::ReviewThread, &'a crate::github::Comment),
    }
    let mut items: Vec<(&str, Item)> = Vec::new();
    for c in d.comments.iter() {
        items.push((&c.created_at, Item::Comment(c)));
    }
    for r in d.reviews.iter() {
        // Empty "COMMENTED" reviews are just containers for inline comments.
        if r.state == "COMMENTED" && clean_body(&r.body).trim().is_empty() {
            continue;
        }
        items.push((r.submitted_at.as_deref().unwrap_or(""), Item::Review(r)));
    }
    for t in d.review_threads.iter() {
        for c in t.comments.iter() {
            items.push((&c.created_at, Item::Inline(t, c)));
        }
    }
    items.sort_by(|a, b| a.0.cmp(b.0));
    section(rows, &format!("Activity ({})", items.len()), w);
    if items.is_empty() {
        rows.push((Line::styled("No comments yet. Press c to add one.", Style::new().fg(DIM).italic()), Vec::new()));
    }
    for (ts, item) in items {
        rows.push((Line::raw(""), Vec::new()));
        let (author, what, body, html): (&str, Span<'static>, &str, &str) = match item {
            Item::Comment(c) => (login_of(&c.author), Span::styled("commented", gray), &c.body, &c.body_html),
            Item::Review(r) => {
                let s = match r.state.as_str() {
                    "APPROVED" => Span::styled("✓ approved", Style::new().fg(Color::Green).bold()),
                    "CHANGES_REQUESTED" => Span::styled("± requested changes", Style::new().fg(Color::Red).bold()),
                    "DISMISSED" => Span::styled("reviewed (dismissed)", Style::new().fg(DIM)),
                    _ => Span::styled("reviewed", gray),
                };
                (login_of(&r.author), s, &r.body, &r.body_html)
            }
            Item::Inline(t, c) => {
                let loc = match t.line {
                    Some(l) => format!("on {}:{l}", t.path),
                    None => format!("on {}", t.path),
                };
                let mut s = loc;
                if t.is_resolved {
                    s.push_str(" ✓ resolved");
                }
                (login_of(&c.author), Span::styled(s, Style::new().fg(Color::Yellow)), &c.body, &c.body_html)
            }
        };
        push_wrapped(
            rows,
            Line::from(vec![
                Span::styled("▍", Style::new().fg(ACCENT)),
                Span::styled(author.to_string(), Style::new().fg(Color::Blue).bold()),
                Span::raw(" "),
                what,
                Span::styled(format!(" · {} ago", ago(ts)), Style::new().fg(DIM)),
            ]),
            w,
            "▍ ",
            None,
        );
        if !clean_body(body).trim().is_empty() {
            body_lines(body, html, w, "▍ ", rows);
        }
    }
    rows.push((Line::raw(""), Vec::new()));
    rows.push((Line::styled("— end —", Style::new().fg(DIM)).centered(), Vec::new()));
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

/// Render a body from GitHub's rendered HTML, falling back to the raw markdown.
fn body_lines(markdown: &str, html: &str, w: usize, indent: &str, rows: &mut Vec<DetailRow>) {
    if html.trim().is_empty() || !html_lines(html, w, indent, rows) {
        markdown_lines(&clean_body(markdown), w, indent, rows);
    }
}

/// Render HTML to styled rows with clickable link ranges. Returns false if it couldn't be parsed.
fn html_lines(html: &str, w: usize, indent: &str, rows: &mut Vec<DetailRow>) -> bool {
    use html2text::render::RichAnnotation as A;
    // Images with no alt text would otherwise render as nothing; <sup>/<sub> render as ^{…}.
    let html = html.replace("alt=\"\"", "alt=\"image\"");
    let html = ["<sup>", "</sup>", "<sub>", "</sub>"].iter().fold(html, |h, t| h.replace(t, ""));
    let indent_w = indent.width();
    let width = w.saturating_sub(indent_w).max(10);
    let Ok(lines) = html2text::config::rich().lines_from_read(html.as_bytes(), width) else { return false };
    let mut out: Vec<DetailRow> = Vec::new();
    for tl in lines {
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
        let blank = text.trim().is_empty();
        if blank && out.last().is_none_or(|(l, _)| l.to_string().trim_start_matches(indent).trim().is_empty()) {
            continue;
        }
        out.push((Line::from(spans), links));
    }
    while out.last().is_some_and(|(l, _)| l.to_string().trim_start_matches(indent).trim().is_empty()) {
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
    let matches = scope_matches(&app.scopes, filter);
    let area = centered(f.area(), 64, matches.len() as u16 + 7);
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

fn draw_help(f: &mut Frame, app: &mut App) {
    let rows: &[(&str, &str)] = &[
        ("", "Everywhere"),
        ("s / click scope", "Choose the scope (account, org, review queue…)"),
        ("f / click state", "Cycle Open → Closed → Merged → All"),
        ("r  F5", "Refresh now (auto-refreshes in the background)"),
        ("/", "Filter the loaded list by title, repo, author or #"),
        ("o", "Open the selected PR in your browser"),
        ("y", "Copy `gh pr checkout` command to clipboard"),
        ("?", "This help · Ctrl+C quits"),
        ("", "List"),
        ("↑↓ j k  PgUp PgDn  g G", "Move selection"),
        ("Enter / double-click", "Open the PR detail view"),
        ("", "Detail view"),
        ("Tab  h l  ← →  click", "Switch focus between list and details"),
        ("j k / wheel", "Scroll details (or move in the list when it has focus)"),
        ("J K", "Next / previous PR while reading details"),
        ("c", "Comment (type @ for mention autocomplete)"),
        ("m", "Merge menu (methods allowed by the repo)"),
        ("a  /  R", "Approve  /  Request changes"),
        ("x", "Close or reopen the PR"),
        ("Space  .", "All actions"),
        ("Esc  q", "Back to the full list"),
        ("", "Mouse"),
        ("click", "Select rows, buttons, menu items, check links"),
        ("click outside", "Dismiss a popup"),
    ];
    let area = centered(f.area(), 76, rows.len() as u16 + 4);
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
    fn wraps_words() {
        let out = wrap(Line::raw("hello world foo"), 11, "  ");
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].to_string(), "  foo");
    }
}
