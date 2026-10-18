mod app;
mod config;
mod editor;
mod github;
mod ui;

use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture};
use ratatui::crossterm::execute;
use tokio::sync::mpsc;

use app::{App, Msg};

#[tokio::main]
async fn main() -> Result<()> {
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
    let input_tx = tx.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if input_tx.send(Msg::Input(ev)).is_err() {
                break;
            }
        }
    });

    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        hook(info);
    }));

    let mut app = App::new(client, tx, cfg);
    app.start();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let result = loop {
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
