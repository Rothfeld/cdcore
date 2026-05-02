use std::io;
use std::sync::Arc;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Terminal,
};

use crate::fs::SharedFs;

pub enum Action { Commit, Abort }

pub fn run(mount: &str, shared: Arc<SharedFs>) -> Action {
    terminal::enable_raw_mode().expect("enable raw mode");
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, terminal::EnterAlternateScreen).ok();

    let backend  = CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend).expect("create terminal");

    let action = event_loop(mount, &shared, &mut term);

    crossterm::execute!(io::stdout(), terminal::LeaveAlternateScreen).ok();
    terminal::disable_raw_mode().ok();
    action
}

fn event_loop(
    mount: &str,
    shared: &Arc<SharedFs>,
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Action {
    loop {
        let pending = shared.pending_write_paths();
        term.draw(|f| draw(f, mount, &pending)).ok();

        if event::poll(Duration::from_millis(250)).unwrap_or(false) {
            if let Ok(Event::Key(KeyEvent { code, modifiers, .. })) = event::read() {
                match (code, modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::NONE)
                    | (KeyCode::Char('C'), KeyModifiers::NONE) => return Action::Commit,
                    (KeyCode::Char('q'), _)
                    | (KeyCode::Char('Q'), _)
                    | (KeyCode::Esc,      _)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Action::Abort,
                    _ => {}
                }
            }
        }
    }
}

fn draw(f: &mut ratatui::Frame, mount: &str, pending: &[String]) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Min(0),     // body
            Constraint::Length(3),  // footer
        ])
        .split(area);

    // ── Header ────────────────────────────────────────────────────────────────
    let header = Paragraph::new(Line::from(vec![
        Span::raw(" "),
        Span::styled("cdfuse", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(mount, Style::default().fg(Color::Cyan)),
        Span::raw("  (rw)"),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(header, chunks[0]);

    // ── Body ──────────────────────────────────────────────────────────────────
    let items: Vec<ListItem> = if pending.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            " No pending writes.",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        pending.iter().map(|p| {
            ListItem::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(p, Style::default().fg(Color::Yellow)),
            ]))
        }).collect()
    };

    let title = if pending.is_empty() {
        " Pending writes ".to_string()
    } else {
        format!(" Pending writes: {} ", pending.len())
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(list, chunks[1]);

    // ── Footer ────────────────────────────────────────────────────────────────
    let footer = Paragraph::new(Line::from(vec![
        Span::raw("  "),
        Span::styled("[c]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw(" commit and repack    "),
        Span::styled("[q]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Span::raw(" quit without saving"),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}
