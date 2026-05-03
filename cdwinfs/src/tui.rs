use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ratatui::crossterm::{
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
    ratatui::crossterm::execute!(stdout, terminal::EnterAlternateScreen)
        .expect("enter alternate screen");

    let backend  = CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend).expect("create terminal");

    let action = event_loop(mount, &shared, &mut term);

    ratatui::crossterm::execute!(io::stdout(), terminal::LeaveAlternateScreen)
        .expect("leave alternate screen");
    terminal::disable_raw_mode().expect("disable raw mode");
    action
}

fn event_loop(
    mount: &str,
    shared: &Arc<SharedFs>,
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Action {
    let saving = Arc::new(AtomicBool::new(false));

    loop {
        let pending   = shared.pending_write_paths();
        let is_saving = saving.load(Ordering::Relaxed);
        term.draw(|f| draw(f, mount, &pending, is_saving)).ok();

        if event::poll(Duration::from_millis(250)).unwrap_or(false) {
            if let Ok(Event::Key(KeyEvent { code, modifiers, .. })) = event::read() {
                match (code, modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::NONE)
                    | (KeyCode::Char('C'), KeyModifiers::NONE) => return Action::Commit,

                    (KeyCode::Char('s'), _) | (KeyCode::Char('S'), _)
                        if !is_saving && !pending.is_empty() =>
                    {
                        let shared2 = Arc::clone(shared);
                        let flag    = Arc::clone(&saving);
                        flag.store(true, Ordering::Relaxed);
                        std::thread::spawn(move || {
                            shared2.flush_all_pending();
                            flag.store(false, Ordering::Relaxed);
                        });
                    }

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

fn draw(f: &mut ratatui::Frame, mount: &str, pending: &[String], saving: bool) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    // -- Header ----------------------------------------------------------------
    let header = Paragraph::new(Line::from(vec![
        Span::raw(" "),
        Span::styled("cdfuse", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(mount, Style::default().fg(Color::Cyan)),
        Span::raw("  (rw)"),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(header, chunks[0]);

    // -- Body ------------------------------------------------------------------
    let (title, items) = if saving {
        let item = ListItem::new(Line::from(Span::styled(
            " Repacking to PAZ...",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        (" Saving ".to_string(), vec![item])
    } else if pending.is_empty() {
        let item = ListItem::new(Line::from(Span::styled(
            " No pending writes.",
            Style::default().fg(Color::DarkGray),
        )));
        (" Pending writes ".to_string(), vec![item])
    } else {
        let rows = pending.iter().map(|p| {
            ListItem::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(p, Style::default().fg(Color::Yellow)),
            ]))
        }).collect();
        (format!(" Pending writes: {} ", pending.len()), rows)
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(list, chunks[1]);

    // -- Footer ----------------------------------------------------------------
    let s_style = if saving || pending.is_empty() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    };
    let spans = vec![
        Span::raw("  "),
        Span::styled("[s]", s_style),
        Span::raw(" save    "),
        Span::styled("[c]", if pending.is_empty() && !saving {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        }),
        Span::raw(" commit and exit    "),
        Span::styled("[q]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Span::raw(" quit without saving"),
    ];

    let footer = Paragraph::new(Line::from(spans))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}
