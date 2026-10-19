# Inline images — design

Request: "on supported terminal, can we render images that are in description / conversations?"
Everything below marked *observed* was checked read-only on 2026-10-19 with `gh api graphql` / `curl` against stealth-factory PRs and a public ratatui PR; crate facts come from the crate sources in `~/.cargo/registry`.

## 0. What the bodies actually contain (observed)

| Form in `bodyHTML` | Where it comes from | Download |
|---|---|---|
| `https://private-user-images.githubusercontent.com/<uid>/<n>-<uuid>.<ext>?jwt=…` | every `user-attachments` upload, **public repos too** (ratatui#1339 renders the same way) | 200 **without auth** while the jwt is valid; the jwt is minted per API call with `exp − nbf = 300 s`, so it is dead by the time a user scrolls to it |
| `https://github.com/user-attachments/assets/<uuid>` | the raw markdown `body`; the `<uuid>` is the same one that appears in the jwt path | 302 → S3 signed URL → 200 **without auth**, also for an asset in a private repo (cphk#142); with `Authorization` it still works, reqwest 0.13.5 strips the header on the cross-host redirect (`redirect.rs::remove_sensitive_headers`) |
| `https://github.com/<o>/<r>/blob/<ref>/<path>?raw=true` (and `/raw/`) | images committed to the repo and linked from the PR (form-atlas#104) | 404 with or without a Bearer on github.com; **200 only as `https://raw.githubusercontent.com/<o>/<r>/<ref>/<path>` with `Authorization: Bearer <token>`** (404 without) |
| `https://camo.githubusercontent.com/<hmac>/<hex-url>` with `data-canonical-src` | every bot badge (631 of 631 `<img>` across 60 org PRs: coderabbit, vercel, greptile); nearly all are **SVG**, 16–32 px high | 200, no auth, no redirect |

Consequences: canonicalise `private-user-images…/<n>-<uuid>.<ext>` → `https://github.com/user-attachments/assets/<uuid>` (stable key, no expiry, cacheable, and the URL to give the browser); rewrite `blob/raw` links to `raw.githubusercontent.com`; send the token **only** to `raw.githubusercontent.com`, `api.github.com` and `github.com` (never to camo or third parties); bot badges are SVG and tiny, so they stay text labels — no SVG decoder needed.

## 1. Crate: `ratatui-image` 11.1.0 (stable), no chafa, own `image` features

* **Version.** `ratatui-image = { version = "11.1.0", default-features = false, features = ["crossterm"] }`. It requires `ratatui ^0.30.1` (we lock 0.30.2); crossterm 0.29 is only a dev-dependency of the crate, the backend comes through `ratatui/crossterm`, so there is no version clash. 11.1.0 (2026-09-16) already has everything we use: the `sliced` module (partial rows for every protocol, added in 11.0.0), one Kitty unicode placeholder per cell (no `skip` cells for Kitty), `SlicedProtocol::new_with_resize`, `Picker::tmux_detected`. 12.0.0-rc.3 (2026-10-18, one day old) only adds `QueryStdioOptions::stdio` + `stdio_dev_tty()` and renames `timeout: Duration` → `timeout_ms: i32`; move to 12.0.0 when it is stable, the rest of the API is identical.
* **Default features are wrong for us.** `default = ["image-defaults", "crossterm", "chafa-dyn"]`: `chafa-dyn` dynamically links `libchafa.so` (would break the npm/Homebrew/.deb binaries), `image-defaults` pulls every codec incl. AVIF/EXR/TIFF. Hence `default-features = false` and our own `image = { version = "0.25", default-features = false, features = ["png", "jpeg", "gif", "webp"] }` (`ratatui-image` itself only enables `image/png`). GIF decodes to its first frame with `image::load_from_memory`; no animation. No `rayon` (decoding runs in `spawn_blocking`, the pool is enough).
* **SVG: skip.** `resvg`+`usvg`+font loading would add ~2–3 MB and the only SVGs in practice are bot badges, which §5 keeps as text anyway.
* **Binary size (measured, release profile of this repo: lto, codegen-units=1, strip):** a ratatui-only probe is 381 KB, the same probe with `ratatui-image` + `image[png,jpeg,gif,webp]` is 1,042 KB → **≈ +0.66 MB** on today's 7.9 MB binary. Dependency count 140 → 154 (`icy_sixel`, `base64-simd`, `rustix 0.38`, `self_cell`, `png`, `zune-jpeg`, `gif`, `image-webp`, …).
* **Protocol detection = terminal queries on stdin.** `Picker::from_query_stdio()` enables raw mode on its own, writes `ESC_G…a=q` (Kitty), `ESC[c` (DA1 → Sixel), `ESC[16t` (cell size in px), terminated by `ESC[5n` (DSR, answered by every terminal), then polls stdin up to `timeout` (default 2000 ms). Replies go to the same tty input queue our input thread reads from — the exact bug `terminal.clear()` hit — so **detection must run before `std::thread::spawn(event::read loop)` in `main.rs`**, right after `resolve_token()` and before `ratatui::init()`, with `timeout = 700 ms`. A reply arriving after the timeout would be parsed by crossterm as a junk key event; with 700 ms on a local terminal that is not realistic (DSR answers in <5 ms), through SSH/mux add nothing, it is just a one-off startup cost of ≤700 ms in terminals that never answer. Font size comes from the `ESC[16t` reply; if there is no reply the crate falls back to `TIOCGWINSZ` pixel size (ioctl) and, failing that, 10×20 — both fine for halfblocks, which only need the aspect ratio.

## 2. Choosing a protocol, Herdr, fallback

Config (`config.toml`) and env, env wins:

```toml
images = "auto"        # "auto" | "kitty" | "sixel" | "iterm2" | "halfblocks" | "off"
image_max_rows = 20    # cap per inline image
```
`GH_PR_MANAGER_IMAGES=halfblocks` overrides the file (same values). Resolution in `images::detect(cfg) -> Option<Picker>`:

1. `off` → `None`; nothing is queried, downloaded or built (the `▣ alt` labels stay exactly as today).
2. `kitty | sixel | iterm2` → still run the query (needed for the font size), then `picker.set_protocol_type(forced)`. If the query timed out there is no font size and a forced graphics protocol is downgraded to halfblocks with a toast.
3. `halfblocks` → `Picker::halfblocks()` plus the ioctl font size; no query at all.
4. `auto` → `Picker::from_query_stdio_with_options(timeout 700 ms)`. A Kitty/Sixel reply is the pass-through proof: the reply can only arrive if the multiplexer forwarded the query out and the answer back in, so a mux that answers will also forward the image data. The crate already wraps everything in tmux DCS pass-through when `TERM`/`TERM_PROGRAM` say tmux. No reply or `Halfblocks` → halfblocks when `COLORTERM` is `truecolor`/`24bit` or `TERM` contains `256color`; otherwise `off` (a 16-colour halfblock image is noise).

**Herdr** (`TERM_PROGRAM=herdr`, `TERM=xterm-256color`, `COLORTERM=truecolor`): there is no documented graphics pass-through in herdr 0.9.4, so expect the query to time out and `auto` to land on halfblocks (truecolor, so it looks fine: one cell = 1×2 "pixels", a 60-column image is 60×40 px). If a later Herdr forwards Kitty queries, `auto` picks it up without a code change. Run directly in Ghostty/Kitty → Kitty protocol; WezTerm/iTerm2 → iTerm2 (the crate blacklists Kitty/Sixel on WezTerm because its placeholder support is incomplete); Konsole/foot/xterm with sixel → Sixel. Status line shows the active protocol (`img: kitty`) so the user can see what was detected; `?` help lists the config values.

## 3. Layout: images inside the pre-wrapped row list

Keep `DetailRow = (Line, Vec<(u16,u16,String)>)` untouched (≈40 call sites) and add a **side table of placements** per built block:

```rust
pub struct ImagePlacement { pub key: ImageKey, pub row: usize, pub rows: u16, pub cols: u16, pub x: u16, pub alt: String }
pub type Built = (Vec<DetailRow>, Vec<ImagePlacement>);   // tab_cache / detail_cache value
```
* `html_lines` emits, for an `<img>` that is **alone on its html2text line** (see §5 for the rule), either one placeholder row `▣ alt (loading…)` / `▣ alt (failed: …)` or, when the store already knows the pixel size, `rows` blank rows; every such row carries a full-row link `#image:<key>` (clicks reuse the existing link path, §5). The placement records the first row index and `x = indent_w` (comments are indented by `▍ `).
* **Size** (`images::fit(dims, font, max_cols, max_rows) -> Size`): `cols = min(max_cols, ceil(w / font.w))` (never upscale), `rows = ceil(cols·font.w·h / (w·font.h))`; if `rows > max_rows` then `rows = max_rows` and `cols` shrinks to keep the aspect. `max_cols = text_w − indent_w`, `max_rows = min(cfg.image_max_rows, max(6, frame_h·2/5))`. `max_rows` goes into the cache keys next to `text_w` (`"{id}:{text_w}:{max_rows}:{tab}:{density}"`), so a vertical resize re-lays out like a horizontal one does today.
* **Rebuild on arrival.** The builders are pure in `(detail, width, store)`; when `Msg::Image*` changes a store entry the app does `tab_cache.clear(); detail_cache = None;` (one rebuild per burst — `main.rs` drains the channel before drawing). Viewport anchoring: `draw_detail` compares the new placements with the previous frame's (`app.last_placements`) by key, and for every placement whose old `row < scroll`, adds `new.rows − old.rows` to `detail_scroll`/`info_scroll`, so content below an image that just loaded does not jump.
* **Drawing** (`render_rows` gains `store: &mut ImageStore, modal_open: bool, placements: &[ImagePlacement]`): after the `Paragraph`, for each placement intersecting `[scroll, scroll + height)` and `Ready`, render `SlicedImage::new(proto, SignedPosition { x, y: row as i16 − scroll as i16 })` over `text_area`. `SlicedImage` clips negative `y` and overflow at the bottom itself for **all four protocols**: Kitty via per-row placeholder diacritics, Sixel by dropping six-pixel bands, iTerm2 as one base64-PNG strip per row, halfblocks per row. So partially visible images are simply cropped — no "placeholder until fully visible" special case. If the encoded protocol for `(key, cols, rows)` is not there yet (width just changed), draw the reserved rows as a dim `▒` box with the alt text centred and ask the store to encode.
* **`sanitize()` must skip image cells.** Sixel/iTerm2 put the whole escape sequence into the first cell's symbol (`cell.set_symbol(data)` + `CellDiffOption::ForcedWidth(1)`, the rest of the area `CellDiffOption::Skip`), and Kitty's first cell carries the transmit sequence; `c.is_control()` matches `ESC`, so today's `sanitize()` would blank them. Rule: `if sym.starts_with('\u{1b}') || sym.contains('\u{10EEEE}') { continue; }`. Halfblocks are plain `▀` cells and need nothing.
* **`dim()` must not see image cells.** Kitty encodes the image id in the placeholder cell's **fg colour**; `dim()` rewrites fg → wrong/invisible image, and a re-emitted sixel would paint over the modal. So `render_rows` draws no image when `modal_open` (the reserved rows show the `▒ alt` box, which dims fine). `layout_key()` already includes `modal.is_some()`, so opening/closing a modal triggers the full repaint that wipes sixel pixels left under it.
* **Full-repaint trick & resize.** The "every cell is `' '` fg Indexed(1)" frame overwrites the placeholder/escape cells; the next frame puts them back and ratatui's diff re-emits them (Kitty: placeholders only, the image data stays in the terminal; Sixel/iTerm2: the sequence is re-sent). That is exactly what we want after a layout change. On a resize `text_w` changes → cache rebuild → new `(cols, rows)` → `Encoding` state until the re-encode lands. A `Protocol` is never resized in place.
* **Density** only changes `gap()`/`text_pad()`, already in the cache key; images follow the text column.
* The **Changes** tab and the diff rows get no images (patches are text).

## 4. Loading pipeline

```rust
pub enum ImageState { Queued, Fetching, Decoding, Ready { dims: (u32,u32), image: Arc<DynamicImage>, protos: Vec<(Size, Arc<SlicedProtocol>)> }, Encoding { dims: (u32,u32), image: Arc<DynamicImage> }, Failed(String), Skipped(SkipReason /* TooSmall, Svg, NotImage */) }
```
* **Fetch** (`github.rs`): `Client::fetch_image(&self, url: &str) -> Result<(Vec<u8>, String /*content-type*/)>`: canonicalise (§0), `Accept: image/*`, 15 s total timeout, bearer only for the allow-listed hosts, refuse `Content-Length > 10 MiB` and stop streaming at 10 MiB, refuse non-`image/*` (a 200 HTML login page), map `image/svg+xml` → `Skipped(Svg)`. Runs in `tokio::spawn`; at most **3 in flight**, FIFO queue in the store.
* **Decode** in `spawn_blocking`: `ImageReader::new(Cursor).with_guessed_format()?.into_dimensions()` first (cheap; `< 64×64 px` → `Skipped(TooSmall)` without decoding), then `decode()`, then `thumbnail(1600, 1600)` when larger (a 3456×1988 screenshot is 27 MB RGBA; 1600 px is wider than any pane will be at 8 px/cell). The decoded image is kept (`Arc<DynamicImage>`) so a width change re-encodes without re-downloading. Then encode straight away for the size the requester asked for: `SlicedProtocol::new_with_resize(&picker, img, size, Resize::Fit(Some(FilterType::Triangle)))` — Sixel/halfblock encoding is CPU-heavy, so it stays off the UI thread. `Picker` is `Clone + Send`; the protocol types are `Send`. Result: `Msg::Image { key, res: Result<Decoded> }`; re-encodes: `Msg::ImageEncoded { key, size, proto }`.
* **Memory LRU:** `entries` touched on every draw; evict least-recently-drawn `Ready/Encoding` entries beyond **64 MiB of decoded pixels or 64 entries** (encoded protos are a fraction of that; Kitty keeps its copy in the terminal, which Kitty caps and evicts itself — the crate sends no `a=d` delete, acceptable). Entries of PRs not viewed for 10 min are dropped on the tick.
* **Disk cache** (follow-up, cheap because the key is canonical): raw bytes at `$XDG_CACHE_HOME/gh-pr-manager/images/<sha256(key)>`, 200 MiB / 500 files, oldest-mtime pruning in a blocking task at startup. Camo and `user-attachments` bytes are immutable, so no validation needed.
* **Lazy + prefetch:** the builders never trigger I/O. After building, `draw_detail` calls `store.want(&placements, scroll, height)`: placements inside `[scroll − height, scroll + 2·height)` are enqueued first, the rest of the current tab after them; nothing from other PRs/tabs. Switching PR leaves the queue, in-flight downloads finish and land in the LRU (cheap, and the user often comes back).
* Placeholder rows are 1 row until `dims` are known, so a `Skipped`/`Failed` image never changes the layout; only `Ready/Encoding` grows it (§3 anchoring).

## 5. Interaction

* **Click an image** (any of its reserved rows; they carry `#image:<key>` as a full-row link): `App::run(Action::OpenUrl)` is reached from the two click sites in `on_mouse`; intercept `#image:` there → `Action::ViewImage(key)` → `Modal::Image { key, alt }`. The modal (`draw_image_modal`) is `centered(area, w−4, h−4)` with `modal_block(alt)`, the image fitted to the inner rect via a second `(key, size)` encode (`Encoding` box until it lands), footer `o browser · y copy url · Esc close`. `draw()` calls `dim()` **before** the modal, so the modal's image cells are never dimmed. `o` opens the canonical URL (`github.com/user-attachments/assets/…`, which the browser can open when logged in; raw.githubusercontent links need the browser session too — fine). `Enter` on the modal also opens the browser. Mouse wheel/Esc/click outside close it.
* **Toggle key `I`** (Shift+i; `i` is info-collapse, `I` is free): flips `store.enabled`, clears the caches, toast `Images off`/`Images on (kitty)`; persisted as `images_enabled` in `state.json`. With `images = "off"` in config the key only toasts `Images disabled in config`. Help gets `I  Show / hide inline images (click one to enlarge)`.
* **Keyboard**: the detail pane is a row list; `j/k` scroll through image rows like text. No image focus/selection in v1.
* **Bot badges and inline images stay text.** An `<img>` becomes an inline image only if (a) it is the only non-blank content of its html2text line (the GitHub paragraph-with-lone-image case; Vercel's `<img> Preview …` status lines and coderabbit's `[<img>](link)` buttons fail this), (b) its `width`/`height` attribute, when present, is ≥ 64 px (`width="16"`, `height="32"`), (c) `src`/`data-canonical-src` does not end in `.svg` and the content type is not SVG, (d) the decoded size is ≥ 64×64. html2text drops attributes, so `html_lines` runs a tiny regex pre-pass over the raw HTML collecting `src → (width, height, data-canonical-src)` before calling html2text; (a) is decided from the tagged line, (d) in the decoder. Rejected images keep today's `▣ alt` magenta label, clickable to the browser.
* **`<details>`**: html2text 0.17.3 has no `details`/`summary` handling and renders the content inline, so images inside collapsed sections render like any other (slightly noisy for CodeRabbit walkthroughs, but those are SVG badges and stay text). **Tables**: an image in a table cell shares its line with `│` borders and other cells → rule (a) keeps it a label.

## 6. Rust-level additions

**`src/images.rs`** (new, ~350 lines):
```rust
pub type ImageKey = String;                      // canonical URL (§0)
pub fn canonical(src: &str) -> (ImageKey, Option<String> /*fetch url*/, bool /*send token*/);
pub fn detect(cfg: &Config) -> Option<Picker>;   // §2; call in main before the input thread
pub fn fit(dims: (u32, u32), font: FontSize, max_cols: u16, max_rows: u16) -> Size;
pub struct ImageStore { picker: Option<Picker>, pub enabled: bool, entries: HashMap<ImageKey, Entry>, queue: VecDeque<ImageKey>, inflight: usize, bytes: usize }
impl ImageStore {
    pub fn protocol_label(&self) -> &'static str;                         // status line
    pub fn dims(&self, key: &str) -> Option<(u32, u32)>;                  // builders use this to reserve rows
    pub fn label(&self, key: &str) -> Option<String>;                     // "(loading…)" / "(failed: …)" suffix
    pub fn proto(&mut self, key: &str, size: Size) -> Option<Arc<SlicedProtocol>>; // touches LRU; None => draw the ▒ box
    pub fn want(&mut self, placements: &[ImagePlacement], scroll: usize, height: usize); // enqueue, §4
    pub fn want_size(&mut self, key: &str, size: Size);                   // re-encode request (resize, modal)
    pub fn pump(&mut self, client: &Client, tx: &UnboundedSender<Msg>);  // spawn fetches/encodes up to the caps
    pub fn apply(&mut self, msg: ImageMsg) -> bool;                       // returns true when a layout rebuild is needed
    pub fn evict(&mut self);
}
```
**`app.rs`:** `Msg::Image { key: ImageKey, res: Result<Decoded> }`, `Msg::ImageEncoded { key, size: Size, proto: Arc<SlicedProtocol> }`; `App.images: ImageStore`, `App.last_placements: Vec<ImagePlacement>`; `Modal::Image { key: ImageKey, alt: String }`; `Action::ViewImage(ImageKey)`, `Action::ToggleImages`; `tab_cache: HashMap<String, (Instant, Built)>`, `detail_cache: Option<(String, usize, Instant, Built)>`; `State.images_enabled: bool`; `layout_key()` needs no change (the image modal is covered by `modal.is_some()`). `App::new(client, tx, cfg, picker)`.
**`config.rs`:** `images: String` (default `"auto"`), `image_max_rows: u16` (default 20).
**`main.rs`:** `let picker = images::detect(&cfg);` between `resolve_token` and the input-thread spawn.
**`ui.rs`:** `html_lines(html, w, indent, rows, images: &mut Vec<ImagePlacement>, store: &ImageStore)` and the same extra parameters down `body_lines` → `entry` → `description_lines`/`conversation_lines`/`reviews_lines`/`info_lines`; `render_rows(f, area, rows, placements, scroll, store, modal_open)`; `sanitize()` skip rule; `draw_image_modal`; status line protocol label.
**`github.rs`:** `Client::fetch_image` (§4); the client adds the bearer itself for allow-listed hosts, so the token never leaves `github.rs`.

**Tests** (`#[cfg(test)]`, no terminal needed; `Picker::from_fontsize`/`Picker::halfblocks` give deterministic pickers):
1. `canonical()`: jwt URL → `user-attachments/assets/<uuid>`, no token; `blob/…?raw=true` and `/raw/` → `raw.githubusercontent.com`, token; camo and arbitrary hosts unchanged, no token; `.svg` and `data-canonical-src` svg flagged.
2. `fit()`: aspect kept with a 10×20 font, no upscaling, `max_rows` cap shrinks `cols`, 1×1 and 0-size inputs.
3. `html_lines`: a lone `<img>` → one placement + 1 placeholder row with a `#image:` full-row link; the same with a store reporting `dims` → `rows` blank rows; `<img width="16">`, an `<img>` inside `<a>…</a> text` on one line, and an `<img>` in a `<td>` → label only, no placement.
4. `sanitize()` leaves an `ESC`-prefixed symbol and a `U+10EEEE` symbol untouched, still fixes `U+FE0F`.
5. Scroll anchoring: a placement above the viewport growing 1 → 12 rows adds 11 to the scroll; below the viewport adds 0.
6. `ImageStore`: concurrency cap of 3, LRU eviction by bytes, `apply(Failed)` keeps 1 row, `Skipped(TooSmall)` for 40×40.

## Risks
* Herdr may forward the query but mangle the image data, or answer DA1 with sixel support it does not have; `images = "halfblocks"` in config is the escape hatch, and the status line shows what was chosen.
* `tab_cache` is never pruned today; clearing it on image arrival also keeps it small, but a conversation with 80 comments is re-rendered through html2text on every arrival burst (tens of ms, acceptable).
* `user-attachments` assets downloading without auth is observed behaviour, not documented; if GitHub tightens it, the fallback is the jwt URL from the `bodyHTML` we already hold (valid ≤300 s after the detail fetch), then `Failed` with a browser link.
* Kitty images accumulate in the terminal (no delete command from the crate); Kitty/Ghostty cap and evict on their side.
* 12.0.0 may rename `QueryStdioOptions.timeout`; the call site is one line in `images::detect`.

## Implementation notes

Deviations from the design above, as built (2026-10-19):

* **Detection uses its own stdin reader, after `ratatui::init()`.** `Picker::from_query_stdio_with_options` reads replies on a helper thread that outlives its timeout: when no reply comes it keeps reading stdin (eating key presses), and a late reply makes it restore the termios it saw at the start, undoing raw mode. `images::query` writes the crate's own query string (`cap_parser::Parser::query`, with its WezTerm/Konsole blacklist) and parses replies with the crate's `Parser`, but reads stdin with `rustix::event::poll` + `rustix::io::read` (no buffering, nothing left reading after 700 ms). That needs raw mode already on (a cooked tty neither delivers the replies before a newline nor keeps them off the screen), so `main.rs` now runs `ratatui::init()` → `images::detect()` → input thread. `rustix` 1.x was already linked through crossterm; it is now a direct `cfg(unix)` dependency with `event` + `termios`. The picker is then built with the deprecated `Picker::from_fontsize` (behind `#[allow(deprecated)]`), the only constructor that takes a cell size. Off Unix there is no query: `auto` lands on halfblocks (or off without colours).
* **States.** No separate `Decoding`/`Encoding` states: `Fetching` covers download + decode, and a `Ready` entry tracks the sizes it still has to encode (`want`/`encoding`). Eviction does not delete an entry but turns it into `Evicted { dims }`: the layout keeps its rows (no jump), the rows show the `▒` box, and the image is fetched again only once it is near the viewport. Without that, a tab whose images exceed 64 MiB (form-atlas#104: 21 renders at ~10 MB decoded each) thrashed: prefetching far images evicted the visible ones, which re-queued behind them. Only near placements bump the LRU order.
* **Content type.** `image/*`, `application/octet-stream` and a missing type are decoded (the decoder guesses the format, so an HTML page is still `Skipped(NotImage)`); `image/svg+xml` → `Skipped(Svg)`; anything else → `Skipped(NotImage)`.
* **Token.** Only for `raw.githubusercontent.com`, `api.github.com` and `github.com` paths other than `/user-attachments/` (those download anonymously, so the token isn't sent there at all). `fetch_image` checks the host against the allow-list again before adding it.
* **`render_rows` is unchanged** (it serves the info pane, the Changes diff and the Actions message too). Images are drawn by a separate `render_images` right after it, in `draw_detail` only; the Actions tab has no images.
* **Layout key** also includes `images.enabled`, so toggling with `I` repaints every cell (Sixel pixels can outlive their cells).
* **`I`** works everywhere, including the Actions tab (it uses none of `O H E W z Z t ^F`). The status line shows `img: kitty|sixel|iterm2|halfblocks|hidden|off` on the right; clicking it toggles too. The image viewer's keys are `o`/Enter (browser), `y` (copy URL), anything else closes; the wheel and a click outside close it.
* **Viewer** fits the image to the modal without upscaling, so a small screenshot stays at its natural cell size.
* **Binary size**: 8,248,720 → 9,368,096 bytes (+1.07 MiB), more than the probe's +0.66 MB.
* Verified in tmux, so halfblocks only. Kitty, Sixel and iTerm2 rendering, cropping and the modal on those protocols are untested.
