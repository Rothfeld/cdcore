use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, ClearType},
};

use crate::fs::SharedFs;

pub enum Action { Commit, Abort }

pub fn run(mount: &str, shared: Arc<SharedFs>) -> Action {
    let mut out = io::stdout();
    terminal::enable_raw_mode().expect("enable raw mode");
    execute!(out, terminal::EnterAlternateScreen, cursor::Hide).ok();

    let action = event_loop(mount, &shared, &mut out);

    execute!(out, terminal::LeaveAlternateScreen, cursor::Show).ok();
    terminal::disable_raw_mode().ok();
    action
}

fn event_loop(mount: &str, shared: &Arc<SharedFs>, out: &mut impl Write) -> Action {
    loop {
        draw(mount, shared, out);

        if event::poll(Duration::from_millis(500)).unwrap_or(false) {
            if let Ok(Event::Key(KeyEvent { code, modifiers, .. })) = event::read() {
                match code {
                    KeyCode::Char('c') | KeyCode::Char('C') => {
                        return Action::Commit;
                    }
                    KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                        return Action::Abort;
                    }
                    KeyCode::Char('c') if modifiers == KeyModifiers::CONTROL => {
                        return Action::Abort;
                    }
                    _ => {}
                }
            }
        }
    }
}

fn draw(mount: &str, shared: &Arc<SharedFs>, out: &mut impl Write) {
    let pending = shared.pending_write_paths();
    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    queue!(out,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0),
    ).ok();

    // Header
    let title = format!("cdfuse  {}  (rw)", mount);
    queue!(out,
        SetForegroundColor(Color::White),
        Print(&title),
        ResetColor,
        Print("\r\n\r\n"),
    ).ok();

    // Pending writes
    if pending.is_empty() {
        queue!(out,
            SetForegroundColor(Color::DarkGrey),
            Print("  No pending writes.\r\n"),
            ResetColor,
        ).ok();
    } else {
        queue!(out,
            SetForegroundColor(Color::Yellow),
            Print(format!("  Pending writes: {}\r\n\r\n", pending.len())),
            ResetColor,
        ).ok();
        let max_show = (rows as usize).saturating_sub(8);
        for path in pending.iter().take(max_show) {
            let display = truncate(path, cols as usize - 6);
            queue!(out, Print(format!("    {display}\r\n"))).ok();
        }
        if pending.len() > max_show {
            queue!(out, Print(format!("    ... and {} more\r\n", pending.len() - max_show))).ok();
        }
    }

    // Footer
    let footer_row = rows.saturating_sub(3);
    queue!(out,
        cursor::MoveTo(0, footer_row),
        SetForegroundColor(Color::DarkGrey),
        Print("  "),
        ResetColor,
        SetForegroundColor(Color::Green),
        Print("[c]"),
        ResetColor,
        Print(" commit and repack    "),
        SetForegroundColor(Color::Red),
        Print("[q]"),
        ResetColor,
        Print(" quit without saving\r\n"),
    ).ok();

    out.flush().ok();
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { return s.to_string(); }
    format!("...{}", &s[s.len().saturating_sub(max - 3)..])
}
