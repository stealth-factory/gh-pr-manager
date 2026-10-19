//! Inline images in PR bodies and comments (docs/images.md): picking a graphics protocol at
//! startup, the URL and auth rules, sizing in cells, and a store that fetches, decodes and
//! encodes images off the UI thread.

use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};

use image::DynamicImage;
use ratatui::layout::Size;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::sliced::SlicedProtocol;
use ratatui_image::{FilterType, FontSize, Resize};
use tokio::sync::mpsc::UnboundedSender;

use crate::app::Msg;
use crate::config::Config;
use crate::github::Client;

/// Canonical image URL: stable across API calls, cacheable, and what the browser opens.
pub type ImageKey = String;

/// Images smaller than this (px, either side) are icons and badges; they stay text labels.
pub const MIN_PX: u32 = 64;
/// How long startup waits for the terminal to answer the capability query.
const QUERY_TIMEOUT: Duration = Duration::from_millis(700);
const MAX_FETCHES: usize = 3;
/// Decoded images kept in memory, by count and by RGBA bytes.
const MAX_ENTRIES: usize = 64;
const MAX_BYTES: usize = 64 << 20;
/// Decoded images are shrunk to fit this square; wider than any pane at 8 px per cell.
const MAX_DECODED: u32 = 1600;
/// Images not drawn for this long are dropped.
const STALE: Duration = Duration::from_secs(600);
/// Encoded sizes kept per image (the inline one, the viewer's, one left over from a resize).
const MAX_PROTOS: usize = 3;

// ---------- URLs ----------

/// The canonical key for an `<img src>`, the URL to fetch (none for `data:` and relative
/// URLs) and whether the token may go with it.
///
/// `private-user-images…/<n>-<uuid>.<ext>?jwt=…` expires five minutes after the API call that
/// minted it, so it becomes the stable `github.com/user-attachments/assets/<uuid>`, which
/// downloads without auth. Committed images linked as `blob/…?raw=true` or `raw/…` only load
/// from `raw.githubusercontent.com`, with the token. Everything else (camo, third parties)
/// is fetched as is and never sees the token.
pub fn canonical(src: &str) -> (ImageKey, Option<String>, bool) {
    let src = src.trim();
    let Some(rest) = src.strip_prefix("https://").or_else(|| src.strip_prefix("http://")) else {
        return (src.to_string(), None, false);
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let same = |token: bool| (src.to_string(), Some(src.to_string()), token);
    match host.to_ascii_lowercase().as_str() {
        "private-user-images.githubusercontent.com" => match path.rsplit('/').next().and_then(attachment_uuid) {
            Some(uuid) => {
                let key = format!("https://github.com/user-attachments/assets/{uuid}");
                (key.clone(), Some(key), false)
            }
            None => same(false),
        },
        "github.com" if path.starts_with("user-attachments/") => same(false),
        "github.com" => {
            let parts: Vec<&str> = path.splitn(5, '/').collect();
            let raw = match parts[..] {
                [owner, repo, "raw", git_ref, file] => Some((owner, repo, git_ref, file)),
                [owner, repo, "blob", git_ref, file] if query.split('&').any(|q| q == "raw=true") => Some((owner, repo, git_ref, file)),
                _ => None,
            };
            match raw {
                Some((owner, repo, git_ref, file)) => {
                    let key = format!("https://raw.githubusercontent.com/{owner}/{repo}/{git_ref}/{file}");
                    (key.clone(), Some(key), true)
                }
                None => same(true),
            }
        }
        "raw.githubusercontent.com" | "api.github.com" => same(true),
        _ => same(false),
    }
}

/// `<n>-<uuid>.<ext>` → `<uuid>`.
fn attachment_uuid(file: &str) -> Option<&str> {
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    let (_, uuid) = stem.split_once('-')?;
    let ok = uuid.len() == 36 && uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    ok.then_some(uuid)
}

/// Whether a URL names an SVG (by its path; the content type is checked again on download).
pub fn looks_svg(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or("");
    path.to_ascii_lowercase().ends_with(".svg")
}

/// The `width`, `height` and `data-canonical-src` attributes of an `<img>`, which html2text drops.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImgAttrs {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub canonical: Option<String>,
}

/// Every `<img>` in `html`, by `src`.
pub fn img_attrs(html: &str) -> HashMap<String, ImgAttrs> {
    let lower = html.to_ascii_lowercase();
    let mut out = HashMap::new();
    let mut i = 0;
    while let Some(off) = lower[i..].find("<img") {
        let start = i + off + 4;
        let end = lower[start..].find('>').map_or(html.len(), |e| start + e);
        let tag = &html[start..end];
        if let Some(src) = tag_attr(tag, "src") {
            let px = |v: Option<String>| v.and_then(|v| v.trim().trim_end_matches("px").parse::<u32>().ok());
            out.insert(src, ImgAttrs { width: px(tag_attr(tag, "width")), height: px(tag_attr(tag, "height")), canonical: tag_attr(tag, "data-canonical-src") });
        }
        i = end;
    }
    out
}

/// One attribute's value from the inside of a tag, entity-decoded like html2text does.
fn tag_attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(off) = lower[from..].find(name) {
        let at = from + off;
        from = at + name.len();
        // A whole attribute name: `src`, not the tail of `data-canonical-src`.
        if !lower[..at].ends_with(|c: char| c.is_whitespace()) {
            continue;
        }
        let rest = lower[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let v = tag[tag.len() - rest.len()..].trim_start();
        let value = match v.chars().next() {
            Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or(""),
            _ => v.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or(""),
        };
        return Some(value.replace("&amp;", "&").replace("&quot;", "\""));
    }
    None
}

// ---------- sizing ----------

/// Cells for an image of `dims` px: as wide as its pixels (never upscaled), at most
/// `max_cols`, rows keeping the aspect; past `max_rows` the columns shrink instead.
pub fn fit(dims: (u32, u32), font: FontSize, max_cols: u16, max_rows: u16) -> Size {
    let (w, h) = (dims.0 as u64, dims.1 as u64);
    if w == 0 || h == 0 || max_cols == 0 || max_rows == 0 {
        return Size::new(0, 0);
    }
    let (fw, fh) = (font.width.max(1) as u64, font.height.max(1) as u64);
    let mut cols = w.div_ceil(fw).clamp(1, max_cols as u64);
    let mut rows = (cols * fw * h).div_ceil(w * fh).max(1);
    if rows > max_rows as u64 {
        rows = max_rows as u64;
        cols = (rows * fh * w / (h * fw)).clamp(1, max_cols as u64);
    }
    Size::new(cols as u16, rows as u16)
}

/// Where an inline image sits in a block of detail rows. Loading images get one placeholder
/// row; ready ones reserve `rows` blank rows that the image is drawn over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePlacement {
    pub key: ImageKey,
    /// Index of the first reserved row.
    pub row: usize,
    pub rows: u16,
    /// The fitted width once ready; the room available while loading.
    pub cols: u16,
    /// Column inside the text area (past a comment's `▍ ` bar).
    pub x: u16,
    pub alt: String,
}

/// The scroll offset that keeps the same content on screen after a rebuild: every image
/// starting above the viewport that grew (or shrank) moves it by the difference.
pub fn anchor(old: &[ImagePlacement], new: &[ImagePlacement], scroll: usize) -> usize {
    let mut delta = 0isize;
    for (i, o) in old.iter().enumerate() {
        if o.row >= scroll {
            continue;
        }
        // The same image may appear twice; pair up occurrences in order.
        let nth = old[..i].iter().filter(|p| p.key == o.key).count();
        if let Some(n) = new.iter().filter(|p| p.key == o.key).nth(nth) {
            delta += n.rows as isize - o.rows as isize;
        }
    }
    scroll.saturating_add_signed(delta)
}

// ---------- protocol detection ----------

/// What startup decided about images.
pub struct Detection {
    pub picker: Option<Picker>,
    /// Shown as a toast once the UI is up (e.g. a forced protocol had to be downgraded).
    pub note: Option<String>,
    /// Why there are no images, for the `I` key's toast.
    pub off_reason: String,
}

/// Pick the graphics protocol from `images` in the config (`GH_PR_MANAGER_IMAGES` wins).
///
/// Queries the terminal, so this must run with raw mode on and before the input thread
/// starts reading stdin: a reply that arrives later would be read as key presses.
pub fn detect(cfg: &Config) -> Detection {
    let env = std::env::var("GH_PR_MANAGER_IMAGES").ok().filter(|v| !v.trim().is_empty());
    let mode = env.unwrap_or_else(|| cfg.images.clone()).trim().to_ascii_lowercase();
    let off = |reason: &str, note: Option<String>| Detection { picker: None, note, off_reason: reason.into() };
    let forced = match mode.as_str() {
        "off" => return off("images = \"off\" in config", None),
        "halfblocks" => return Detection { picker: Some(picker_with(ProtocolType::Halfblocks, font_from_ioctl())), note: None, off_reason: String::new() },
        "kitty" => Some(ProtocolType::Kitty),
        "sixel" => Some(ProtocolType::Sixel),
        "iterm2" => Some(ProtocolType::Iterm2),
        "auto" => None,
        other => return off("unknown images setting", Some(format!("Unknown images = \"{other}\" (auto, kitty, sixel, iterm2, halfblocks or off); images are off"))),
    };
    // Constructing a picker first also turns on tmux pass-through, which the query needs.
    let base = Picker::halfblocks();
    let reply = query(base.tmux_detected(), QUERY_TIMEOUT);
    let font = reply.font.or_else(font_from_ioctl);
    let ok = |picker| Detection { picker: Some(picker), note: None, off_reason: String::new() };
    match (forced, font) {
        (Some(p), Some(font)) => ok(picker_with(p, Some(font))),
        (Some(_), None) => Detection {
            picker: Some(base),
            note: Some("The terminal didn't report its font size, so images use halfblocks".into()),
            off_reason: String::new(),
        },
        // A Kitty/Sixel reply proves a multiplexer in between forwards graphics too.
        (None, Some(font)) if reply.proto.is_some() || iterm2_hint() => ok(picker_with(reply.proto.unwrap_or(ProtocolType::Iterm2), Some(font))),
        // A 16-colour halfblock image is noise.
        _ if !colour_ok() => off("the terminal reports neither 256 colours nor truecolor (set images = \"halfblocks\" to force)", None),
        _ => ok(picker_with(ProtocolType::Halfblocks, font)),
    }
}

/// A picker for `proto` with the given cell size (halfblocks' 10×20 when unknown).
fn picker_with(proto: ProtocolType, font: Option<FontSize>) -> Picker {
    let mut p = match font {
        Some(f) => picker_for_font(f),
        None => Picker::halfblocks(),
    };
    p.set_protocol_type(proto);
    p
}

/// `from_fontsize` is deprecated in favour of the crate's own stdio query, which `detect`
/// can't use (see `query`), and it is the only way to set the cell size.
#[allow(deprecated)]
fn picker_for_font(font: FontSize) -> Picker {
    Picker::from_fontsize(font)
}

fn colour_ok() -> bool {
    let colorterm = std::env::var("COLORTERM").unwrap_or_default().to_ascii_lowercase();
    colorterm == "truecolor" || colorterm == "24bit" || std::env::var("TERM").is_ok_and(|t| t.contains("256color"))
}

/// Terminals that speak iTerm2's protocol but don't answer the query for it.
fn iterm2_hint() -> bool {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let program = var("TERM_PROGRAM");
    ["iTerm", "WezTerm", "mintty", "vscode", "Tabby", "Hyper", "rio", "Bobcat", "WarpTerminal"].iter().any(|t| program.contains(t))
        || var("LC_TERMINAL").contains("iTerm")
        || !var("WEZTERM_EXECUTABLE").is_empty()
}

#[derive(Default)]
struct Reply {
    proto: Option<ProtocolType>,
    font: Option<FontSize>,
}

/// Send the Kitty / DA1 (Sixel) / cell-size queries, ended by a status report every terminal
/// answers, and read replies until that answer or `timeout`.
///
/// Not `Picker::from_query_stdio_with_options`: its reader thread outlives the timeout, keeps
/// reading stdin (eating key presses) and later restores the termios it saw, undoing raw
/// mode. Here stdin is polled, so nothing is left reading once this returns.
#[cfg(unix)]
fn query(is_tmux: bool, timeout: Duration) -> Reply {
    use ratatui_image::picker::cap_parser::{Parser, QueryStdioOptions, Response};
    use rustix::event::{PollFd, PollFlags, Timespec};
    use std::io::{IsTerminal, Write};
    use std::os::fd::AsFd;

    let mut reply = Reply::default();
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return reply;
    }
    let mut options = QueryStdioOptions::default();
    // Neither implements Kitty's placeholders properly, and Konsole's Sixel is buggy.
    if ["WEZTERM_EXECUTABLE", "KONSOLE_VERSION"].iter().any(|k| std::env::var(k).is_ok_and(|v| !v.is_empty())) {
        options.blacklist_protocols = vec![ProtocolType::Kitty, ProtocolType::Sixel];
    }
    let mut out = std::io::stdout();
    if out.write_all(Parser::query(is_tmux, options).as_bytes()).and_then(|_| out.flush()).is_err() {
        return reply;
    }
    let deadline = Instant::now() + timeout;
    let mut parser = Parser::new();
    let mut buf = [0u8; 256];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return reply;
        }
        let ts = Timespec { tv_sec: left.as_secs() as _, tv_nsec: left.subsec_nanos() as _ };
        match rustix::event::poll(&mut [PollFd::new(&stdin, PollFlags::IN)], Some(&ts)) {
            Ok(0) | Err(rustix::io::Errno::INTR) => continue,
            Ok(_) => {}
            Err(_) => return reply,
        }
        let n = match rustix::io::read(stdin.as_fd(), &mut buf) {
            Ok(n) if n > 0 => n,
            _ => return reply,
        };
        for &b in &buf[..n] {
            for r in parser.push(char::from(b)) {
                match r {
                    Response::Status => return reply,
                    Response::Kitty => reply.proto = Some(ProtocolType::Kitty),
                    Response::Sixel if reply.proto.is_none() => reply.proto = Some(ProtocolType::Sixel),
                    Response::CellSize(Some((w, h))) if w > 0 && h > 0 => reply.font = Some(FontSize::new(w, h)),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(not(unix))]
fn query(_is_tmux: bool, _timeout: Duration) -> Reply {
    Reply::default()
}

/// Cell size from the tty's pixel size, when the terminal fills it in.
#[cfg(unix)]
fn font_from_ioctl() -> Option<FontSize> {
    let ws = rustix::termios::tcgetwinsize(std::io::stdout()).ok()?;
    (ws.ws_xpixel > 0 && ws.ws_ypixel > 0 && ws.ws_col > 0 && ws.ws_row > 0).then(|| FontSize::new(ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row))
}

#[cfg(not(unix))]
fn font_from_ioctl() -> Option<FontSize> {
    None
}

// ---------- store ----------

/// Why an image is shown as a text label instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    TooSmall,
    Svg,
    NotImage,
}

impl SkipReason {
    pub fn label(self) -> &'static str {
        match self {
            SkipReason::TooSmall => "it's smaller than 64 px",
            SkipReason::Svg => "SVGs aren't rendered",
            SkipReason::NotImage => "the server didn't send an image",
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Fail {
    Failed(String),
    Skipped(SkipReason),
}

/// A downloaded image, decoded, plus its first encode (for the size the layout asked for).
pub struct Decoded {
    pub dims: (u32, u32),
    pub image: Arc<DynamicImage>,
    pub first: Option<(Size, SlicedProtocol)>,
}

pub enum ImageMsg {
    Loaded { key: ImageKey, res: Result<Decoded, Fail> },
    Encoded { key: ImageKey, size: Size, res: Result<SlicedProtocol, String> },
}

enum State {
    Queued,
    /// Downloading or decoding.
    Fetching,
    Ready {
        dims: (u32, u32),
        image: Arc<DynamicImage>,
        protos: Vec<(Size, SlicedProtocol)>,
        /// Sizes asked for but not encoded yet, and of those, the ones being encoded.
        want: Vec<Size>,
        encoding: Vec<Size>,
    },
    /// Dropped from memory to stay under the caps. The size is kept so the layout doesn't
    /// move; it's fetched again when it comes back near the screen.
    Evicted { dims: (u32, u32) },
    Failed(String),
    Skipped(SkipReason),
}

struct Entry {
    state: State,
    /// Room the first layout offered, `(cols, rows)`, so the first encode fits it.
    bounds: (u16, u16),
    /// LRU order, and when it was last wanted or drawn.
    used: u64,
    seen: Instant,
}

/// What the row builders need to know about an image.
#[derive(Debug, Clone, PartialEq)]
pub enum Look {
    Pending,
    Ready((u32, u32)),
    Failed(String),
    Skipped(SkipReason),
}

/// Background work `pump` starts.
pub enum Job {
    Fetch { key: ImageKey, url: String, auth: bool, bounds: (u16, u16) },
    Encode { key: ImageKey, size: Size, image: Arc<DynamicImage> },
}

pub struct ImageStore {
    picker: Option<Picker>,
    /// Shown (the `I` key); persisted in state.json.
    pub enabled: bool,
    pub off_reason: String,
    /// Row cap for inline images in the layout being built (depends on the frame height).
    pub max_rows: u16,
    entries: HashMap<ImageKey, Entry>,
    queue: VecDeque<ImageKey>,
    inflight: usize,
    clock: u64,
    max_entries: usize,
    max_bytes: usize,
}

impl ImageStore {
    pub fn new(picker: Option<Picker>, enabled: bool, off_reason: String) -> Self {
        Self {
            picker,
            enabled,
            off_reason,
            max_rows: 20,
            entries: HashMap::new(),
            queue: VecDeque::new(),
            inflight: 0,
            clock: 0,
            max_entries: MAX_ENTRIES,
            max_bytes: MAX_BYTES,
        }
    }

    /// A protocol was picked (images aren't off in config or for lack of colours).
    pub fn available(&self) -> bool {
        self.picker.is_some()
    }

    /// Images render inline right now.
    pub fn inline_ok(&self) -> bool {
        self.enabled && self.picker.is_some()
    }

    /// For the status line: the protocol, `hidden` (toggled off) or `off`.
    pub fn protocol_label(&self) -> &'static str {
        match &self.picker {
            None => "off",
            Some(_) if !self.enabled => "hidden",
            Some(p) => match p.protocol_type() {
                ProtocolType::Kitty => "kitty",
                ProtocolType::Sixel => "sixel",
                ProtocolType::Iterm2 => "iterm2",
                ProtocolType::Halfblocks => "halfblocks",
            },
        }
    }

    pub fn font(&self) -> FontSize {
        self.picker.as_ref().map_or(FontSize::new(10, 20), Picker::font_size)
    }

    pub fn look(&self, key: &str) -> Look {
        match self.entries.get(key).map(|e| &e.state) {
            None | Some(State::Queued | State::Fetching) => Look::Pending,
            Some(State::Ready { dims, .. } | State::Evicted { dims }) => Look::Ready(*dims),
            Some(State::Failed(e)) => Look::Failed(e.clone()),
            Some(State::Skipped(why)) => Look::Skipped(*why),
        }
    }

    fn touch(&mut self, key: &str) -> Option<&mut Entry> {
        self.clock += 1;
        let clock = self.clock;
        let e = self.entries.get_mut(key)?;
        e.used = clock;
        e.seen = Instant::now();
        Some(e)
    }

    /// The image encoded at `size`; when it isn't yet, asks for it and returns `None`.
    pub fn proto(&mut self, key: &str, size: Size) -> Option<&SlicedProtocol> {
        let e = self.touch(key)?;
        let State::Ready { protos, want, .. } = &mut e.state else { return None };
        match protos.iter().position(|(s, _)| *s == size) {
            Some(i) => Some(&protos[i].1),
            None => {
                if !want.contains(&size) {
                    want.push(size);
                }
                None
            }
        }
    }

    /// Queue the images of a laid-out block: those on or near the screen first, then the rest
    /// of it. Only the near ones count as recently used, so they are the last to be evicted,
    /// and only they are fetched again once evicted.
    pub fn want(&mut self, placements: &[ImagePlacement], scroll: usize, height: usize) {
        if !self.inline_ok() {
            return;
        }
        let near = |p: &ImagePlacement| p.row + p.rows as usize > scroll.saturating_sub(height) && p.row < scroll + 2 * height;
        let mut urgent = Vec::new();
        for p in placements.iter().filter(|p| near(p)).chain(placements.iter().filter(|p| !near(p))) {
            let bounds = (p.cols, self.max_rows);
            let is_near = near(p);
            let entry = if is_near { self.touch(&p.key) } else { self.entries.get_mut(&p.key) };
            match entry {
                None => {
                    self.clock += 1;
                    self.entries.insert(p.key.clone(), Entry { state: State::Queued, bounds, used: self.clock, seen: Instant::now() });
                    self.queue.push_back(p.key.clone());
                }
                Some(e) => {
                    e.seen = Instant::now();
                    if is_near && let State::Evicted { .. } = e.state {
                        (e.state, e.bounds) = (State::Queued, (p.cols, p.rows));
                        urgent.push(p.key.clone());
                        self.queue.push_back(p.key.clone());
                    } else if is_near && matches!(e.state, State::Queued) {
                        urgent.push(p.key.clone());
                    }
                }
            }
        }
        // Bring on-screen images still waiting to the front, keeping their order.
        for key in urgent.iter().rev() {
            if let Some(i) = self.queue.iter().position(|k| k == key) {
                self.queue.remove(i);
                self.queue.push_front(key.clone());
            }
        }
    }

    /// Load one image ahead of everything else (the viewer modal).
    pub fn want_now(&mut self, key: &str, bounds: (u16, u16)) {
        if !self.inline_ok() || self.touch(key).is_some() {
            return;
        }
        self.entries.insert(key.to_string(), Entry { state: State::Queued, bounds, used: self.clock, seen: Instant::now() });
        self.queue.push_front(key.to_string());
    }

    /// Take the work that can start now: fetches up to the concurrency cap, and encodes.
    pub fn next_jobs(&mut self) -> Vec<Job> {
        let mut jobs = Vec::new();
        if self.picker.is_none() {
            return jobs;
        }
        while self.inflight < MAX_FETCHES
            && let Some(key) = self.queue.pop_front()
        {
            let Some(e) = self.entries.get_mut(&key).filter(|e| matches!(e.state, State::Queued)) else { continue };
            let (_, url, auth) = canonical(&key);
            match url {
                Some(url) => {
                    e.state = State::Fetching;
                    self.inflight += 1;
                    jobs.push(Job::Fetch { key, url, auth, bounds: e.bounds });
                }
                None => e.state = State::Skipped(SkipReason::NotImage),
            }
        }
        for (key, e) in self.entries.iter_mut() {
            if let State::Ready { image, want, encoding, .. } = &mut e.state {
                for size in want.drain(..) {
                    if !encoding.contains(&size) {
                        encoding.push(size);
                        jobs.push(Job::Encode { key: key.clone(), size, image: image.clone() });
                    }
                }
            }
        }
        jobs
    }

    /// Start whatever `next_jobs` hands out; results come back as `Msg::Image`.
    pub fn pump(&mut self, client: &Client, tx: &UnboundedSender<Msg>) {
        let Some(picker) = self.picker.clone() else { return };
        for job in self.next_jobs() {
            let (client, tx, picker) = (client.clone(), tx.clone(), picker.clone());
            match job {
                Job::Fetch { key, url, auth, bounds } => {
                    tokio::spawn(async move {
                        let res = match client.fetch_image(&url, auth).await {
                            Ok((_, ctype)) if ctype.starts_with("image/svg") => Err(Fail::Skipped(SkipReason::Svg)),
                            Ok((_, ctype)) if !(ctype.is_empty() || ctype.starts_with("image/") || ctype.contains("octet-stream")) => {
                                Err(Fail::Skipped(SkipReason::NotImage))
                            }
                            Ok((bytes, _)) => tokio::task::spawn_blocking(move || decode(&bytes, &picker, bounds))
                                .await
                                .unwrap_or_else(|e| Err(Fail::Failed(e.to_string()))),
                            Err(e) => Err(Fail::Failed(format!("{e:#}"))),
                        };
                        let _ = tx.send(Msg::Image(ImageMsg::Loaded { key, res }));
                    });
                }
                Job::Encode { key, size, image } => {
                    tokio::task::spawn_blocking(move || {
                        let res = encode(&picker, &image, size).map_err(|e| e.to_string());
                        let _ = tx.send(Msg::Image(ImageMsg::Encoded { key, size, res }));
                    });
                }
            }
        }
    }

    /// Record a result; true when the layout has to be rebuilt (an image's rows changed).
    pub fn apply(&mut self, msg: ImageMsg) -> bool {
        match msg {
            ImageMsg::Loaded { key, res } => {
                if self.entries.get(&key).is_some_and(|e| matches!(e.state, State::Fetching)) {
                    self.inflight = self.inflight.saturating_sub(1);
                }
                let state = match res {
                    Ok(d) => State::Ready { dims: d.dims, image: d.image, protos: d.first.into_iter().collect(), want: Vec::new(), encoding: Vec::new() },
                    Err(Fail::Failed(e)) => State::Failed(e),
                    Err(Fail::Skipped(why)) => State::Skipped(why),
                };
                self.clock += 1;
                let entry = self.entries.entry(key).or_insert(Entry { state: State::Queued, bounds: (0, 0), used: 0, seen: Instant::now() });
                entry.state = state;
                entry.used = self.clock;
                self.evict();
                true
            }
            ImageMsg::Encoded { key, size, res } => {
                if let Some(State::Ready { protos, encoding, .. }) = self.entries.get_mut(&key).map(|e| &mut e.state) {
                    encoding.retain(|s| *s != size);
                    if let Ok(p) = res {
                        protos.retain(|(s, _)| *s != size);
                        protos.push((size, p));
                        if protos.len() > MAX_PROTOS {
                            protos.remove(0);
                        }
                    }
                }
                false
            }
        }
    }

    /// Drop images not seen for a while, then the least recently drawn beyond the caps.
    pub fn evict(&mut self) {
        let stale = |e: &Entry| e.seen.elapsed() > STALE && !matches!(e.state, State::Fetching);
        self.entries.retain(|_, e| !stale(e));
        self.queue.retain(|k| self.entries.contains_key(k));
        let bytes = |e: &Entry| match &e.state {
            State::Ready { image, .. } => image.width() as usize * image.height() as usize * 4,
            _ => 0,
        };
        let mut ready: Vec<(u64, usize, ImageKey)> =
            self.entries.iter().filter(|(_, e)| matches!(e.state, State::Ready { .. })).map(|(k, e)| (e.used, bytes(e), k.clone())).collect();
        ready.sort();
        let (mut count, mut total) = (ready.len(), ready.iter().map(|r| r.1).sum::<usize>());
        for (_, b, key) in ready {
            if count <= self.max_entries && total <= self.max_bytes {
                break;
            }
            if let Some(e) = self.entries.get_mut(&key)
                && let State::Ready { dims, .. } = e.state
            {
                e.state = State::Evicted { dims };
            }
            count -= 1;
            total -= b;
        }
    }
}

/// Decode downloaded bytes and encode them for the cells `bounds` offers. Blocking.
pub fn decode(bytes: &[u8], picker: &Picker, bounds: (u16, u16)) -> Result<Decoded, Fail> {
    let failed = |e: image::ImageError| Fail::Failed(e.to_string());
    let reader = || image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| Fail::Failed(e.to_string()));
    let r = reader()?;
    if r.format().is_none() {
        return Err(Fail::Skipped(SkipReason::NotImage));
    }
    // The header alone says whether it's a badge-sized icon; skip decoding those.
    let (w, h) = r.into_dimensions().map_err(failed)?;
    if w < MIN_PX || h < MIN_PX {
        return Err(Fail::Skipped(SkipReason::TooSmall));
    }
    let mut image = reader()?.decode().map_err(failed)?;
    if w > MAX_DECODED || h > MAX_DECODED {
        image = image.thumbnail(MAX_DECODED, MAX_DECODED);
    }
    let dims = (image.width(), image.height());
    let size = fit(dims, picker.font_size(), bounds.0, bounds.1);
    let first = (size.width > 0).then(|| encode(picker, &image, size).ok().map(|p| (size, p))).flatten();
    Ok(Decoded { dims, image: Arc::new(image), first })
}

/// Encode for the given cells (Sixel and halfblocks are CPU-heavy: call off the UI thread).
fn encode(picker: &Picker, image: &DynamicImage, size: Size) -> Result<SlicedProtocol, ratatui_image::errors::Errors> {
    SlicedProtocol::new_with_resize(picker, image.clone(), size, Resize::Fit(Some(FilterType::Triangle)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0";

    pub fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(w, h).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn placement(key: &str, row: usize, rows: u16) -> ImagePlacement {
        ImagePlacement { key: key.into(), row, rows, cols: 40, x: 0, alt: "a".into() }
    }

    fn ready(key: &str, w: u32, h: u32) -> ImageMsg {
        let image = Arc::new(DynamicImage::new_rgba8(w, h));
        ImageMsg::Loaded { key: key.into(), res: Ok(Decoded { dims: (w, h), image, first: None }) }
    }

    #[test]
    fn canonicalises_urls() {
        let jwt = format!("https://private-user-images.githubusercontent.com/12345/498765432-{UUID}.png?jwt=eyJ0eXAi.x.y");
        let key = format!("https://github.com/user-attachments/assets/{UUID}");
        assert_eq!(canonical(&jwt), (key.clone(), Some(key.clone()), false));
        assert_eq!(canonical(&key), (key.clone(), Some(key), false));
        let raw = "https://raw.githubusercontent.com/acme/app/abc123/docs/shot.png".to_string();
        assert_eq!(canonical("https://github.com/acme/app/blob/abc123/docs/shot.png?raw=true"), (raw.clone(), Some(raw.clone()), true));
        assert_eq!(canonical("https://github.com/acme/app/raw/abc123/docs/shot.png"), (raw.clone(), Some(raw), true));
        let camo = "https://camo.githubusercontent.com/abcdef/68747470733a2f2f";
        assert_eq!(canonical(camo), (camo.to_string(), Some(camo.to_string()), false));
        let other = "https://example.com/x.png";
        assert_eq!(canonical(other), (other.to_string(), Some(other.to_string()), false));
        assert_eq!(canonical("data:image/png;base64,AAAA").1, None);
    }

    #[test]
    fn flags_svgs() {
        assert!(looks_svg("https://img.shields.io/badge/x.SVG?style=flat"));
        assert!(!looks_svg("https://example.com/x.png"));
        let attrs = img_attrs(r#"<p><img src="https://camo.githubusercontent.com/a/b" data-canonical-src="https://coderabbit.ai/badge.svg?x=1&amp;y=2" width="16"></p>"#);
        let a = &attrs["https://camo.githubusercontent.com/a/b"];
        assert!(a.canonical.as_deref().is_some_and(looks_svg));
        assert_eq!(a.canonical.as_deref(), Some("https://coderabbit.ai/badge.svg?x=1&y=2"));
        assert_eq!((a.width, a.height), (Some(16), None));
    }

    #[test]
    fn fits_cells() {
        let font = FontSize::new(10, 20);
        // 2:1 picture, 60 columns: 600×300 px of cells.
        assert_eq!(fit((800, 400), font, 60, 20), Size::new(60, 15));
        // Never upscaled: 100 px wide is 10 columns even with room for 60.
        assert_eq!(fit((100, 100), font, 60, 20), Size::new(10, 5));
        // Too tall: the row cap shrinks the columns, keeping the aspect.
        assert_eq!(fit((400, 2000), font, 60, 20), Size::new(8, 20));
        assert_eq!(fit((1, 1), font, 60, 20), Size::new(1, 1));
        assert_eq!(fit((0, 100), font, 60, 20), Size::new(0, 0));
        assert_eq!(fit((100, 100), font, 0, 20), Size::new(0, 0));
    }

    #[test]
    fn anchors_scroll() {
        let old = [placement("a", 5, 1), placement("b", 40, 1)];
        let new = [placement("a", 5, 12), placement("b", 51, 12)];
        // `a` is above the viewport and grew by 11; `b` is below it.
        assert_eq!(anchor(&old, &new, 20), 31);
        assert_eq!(anchor(&old, &new, 0), 0);
        assert_eq!(anchor(&old, &new, usize::MAX), usize::MAX);
    }

    #[test]
    fn caps_concurrent_fetches() {
        let mut store = ImageStore::new(Some(Picker::halfblocks()), true, String::new());
        let ps: Vec<_> = (0..5).map(|i| placement(&format!("https://example.com/{i}.png"), i * 3, 1)).collect();
        store.want(&ps, 0, 40);
        assert_eq!(store.next_jobs().len(), 3);
        assert!(store.next_jobs().is_empty());
        store.apply(ImageMsg::Loaded { key: ps[0].key.clone(), res: Err(Fail::Failed("HTTP 404".into())) });
        assert_eq!(store.look(&ps[0].key), Look::Failed("HTTP 404".into()));
        let jobs = store.next_jobs();
        assert!(matches!(&jobs[..], [Job::Fetch { key, .. }] if *key == ps[3].key));
    }

    #[test]
    fn evicts_least_recently_used_by_bytes() {
        let mut store = ImageStore::new(Some(Picker::halfblocks()), true, String::new());
        store.max_bytes = 3 * 100 * 100 * 4;
        let k = |name: &str| format!("https://example.com/{name}.png");
        for name in ["a", "b", "c"] {
            store.apply(ready(&k(name), 100, 100));
        }
        store.touch(&k("a"));
        store.apply(ready(&k("d"), 100, 100));
        // The least recently used goes first, keeping its size for the layout.
        assert!(matches!(store.entries[&k("b")].state, State::Evicted { dims: (100, 100) }));
        assert_eq!(store.look(&k("b")), Look::Ready((100, 100)));
        assert!(store.proto(&k("b"), Size::new(10, 5)).is_none());
        for name in ["a", "c", "d"] {
            assert!(matches!(store.entries[&k(name)].state, State::Ready { .. }));
        }
        // Back on screen, it is fetched again.
        store.want(&[placement(&k("b"), 0, 5)], 0, 40);
        assert!(matches!(&store.next_jobs()[..], [Job::Fetch { key, .. }] if *key == k("b")));
    }

    #[test]
    fn decodes_and_skips_small_images() {
        let picker = Picker::halfblocks();
        assert!(matches!(decode(&png(40, 40), &picker, (60, 20)), Err(Fail::Skipped(SkipReason::TooSmall))));
        assert!(matches!(decode(b"<html>login</html>", &picker, (60, 20)), Err(Fail::Skipped(SkipReason::NotImage))));
        let d = decode(&png(200, 100), &picker, (60, 20)).ok().unwrap();
        assert_eq!(d.dims, (200, 100));
        assert_eq!(d.first.map(|(s, _)| s), Some(Size::new(20, 5)));
    }
}

