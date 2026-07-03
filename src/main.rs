mod app;
mod docker;
mod platform;
mod ports;
mod services;
mod ui;

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use ratatui::DefaultTerminal;

const EVENT_TICK: Duration = Duration::from_millis(200);

fn main() -> io::Result<()> {
    // Config errors abort before the TUI takes over the screen.
    let manager = services::Manager::load().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let mut app = app::App::new(manager);
    let resumed = app.manager.resume_session();

    let terminal = ratatui::init();
    crossterm::execute!(io::stdout(), EnableMouseCapture)?;
    let res = run(terminal, &mut app, resumed);
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    res
}

fn run(mut terminal: DefaultTerminal, app: &mut app::App, resumed: Vec<String>) -> io::Result<()> {
    app.poll();
    if !resumed.is_empty() {
        app.message = resumed.join("; ");
    }
    let mut last_poll = Instant::now();
    loop {
        terminal.draw(|f| ui::draw(f, app))?;
        if event::poll(EVENT_TICK)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if !app.on_key(key.code) {
                        return Ok(());
                    }
                }
                Event::Mouse(mouse) => app.on_mouse(mouse),
                _ => {}
            }
        }
        // Cheap, runs every tick so a respawn's instant-exit is caught
        // within ~EVENT_TICK rather than waiting for the next full poll.
        app.watch_respawn_crashes();
        app.watch_docker_ops();
        if last_poll.elapsed() >= app::POLL_INTERVAL {
            app.poll();
            last_poll = Instant::now();
        }
    }
}
