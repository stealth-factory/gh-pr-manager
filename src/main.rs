mod actions_log;
mod app;
mod changes;
mod config;
mod editor;
mod github;
mod images;
mod search;
mod stacks;
mod state;
mod ui;

use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture};
use ratatui::crossterm::execute;
use tokio::sync::mpsc;

use app::{App, Msg};

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().any(|a| a == "-V" || a == "--version") {
        // Release tooling parses this exact shape: `gh-pr-manager X.Y.Z (<sha>, <YYYY-MM-DD>)`.
        println!(
            "gh-pr-manager {} ({}, {})",
            env!("CARGO_PKG_VERSION"),
            env!("GH_PR_MANAGER_COMMIT"),
            env!("GH_PR_MANAGER_BUILD_DATE")
        );
        return Ok(());
    }
    if std::env::args().any(|a| a == "-h" || a == "--help") {
        println!(
            "gh-pr-manager — a TUI for browsing and managing GitHub pull requests\n\n\
             Auth: GH_TOKEN / GITHUB_TOKEN, or the GitHub CLI (`gh auth login`).\n\
             Config: {}\n\nPress ? inside the app for keys.",
            config::config_path().map(|p| p.display().to_string()).unwrap_or_default()
        );
        return Ok(());
    }
    let cfg = config::load()?;
    let token = github::resolve_token().await?;
    let client = github::Client::new(token)?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut terminal = ratatui::init();
    // Ask the terminal about graphics now: raw mode is on (so the replies are neither echoed
    // nor line-buffered) and nothing reads stdin yet. Once the input thread runs, a reply
    // would be swallowed as key presses. Never query the terminal after this point.
    let images = images::detect(&cfg);
    let input_tx = tx.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if input_tx.send(Msg::Input(ev)).is_err() {
                break;
            }
        }
    });
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        hook(info);
    }));

    let mut app = App::new(client, tx, cfg, images);
    app.start();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let mut layout = app.layout_key();
    let result = loop {
        let key = app.layout_key();
        if key != layout {
            layout = key;
            // Repaint every cell. Not `terminal.clear()`: it queries the cursor position and
            // waits for the reply on stdin, which the input thread swallows (a ~2s stall).
            // A frame that differs from the last in every cell forces the next one to be
            // written out in full, overwriting anything stale the terminal kept.
            let _ = terminal.draw(|f| {
                for cell in f.buffer_mut().content.iter_mut() {
                    cell.set_symbol(" ").fg = ratatui::style::Color::Indexed(1);
                }
            });
        }
        if let Err(e) = terminal.draw(|f| ui::draw(f, &mut app)) {
            break Err(e.into());
        }
        tokio::select! {
            Some(msg) = rx.recv() => {
                app.handle(msg);
                // Drain whatever else is queued so bursts (e.g. wheel scrolling) render once.
                while let Ok(msg) = rx.try_recv() {
                    app.handle(msg);
                }
            }
            _ = tick.tick() => app.on_tick(),
        }
        if app.quit {
            break Ok(());
        }
    };

    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}
