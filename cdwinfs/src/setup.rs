//! First-run setup TUI: directory browser → mount-point input.
//!
//! Runs before the main filesystem is mounted; returns (game_dir, mount)
//! or None if the user pressed Esc.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use ratatui::{
    backend::CrosstermBackend,
    crossterm::{event::{self, Event, KeyCode, KeyModifiers}, terminal},
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};

// ---- Entry kinds used inside Picker -----------------------------------------

enum EntryKind {
    SelectThis,
    GoUp,
    Enter(PathBuf),
}

// ---- File-system entry displayed in the list --------------------------------

enum Entry {
    SelectThis,
    Parent,
    Drive(PathBuf),
    Dir(PathBuf),
}

impl Entry {
    fn kind(&self) -> EntryKind {
        match self {
            Entry::SelectThis  => EntryKind::SelectThis,
            Entry::Parent      => EntryKind::GoUp,
            Entry::Drive(p)    => EntryKind::Enter(p.clone()),
            Entry::Dir(p)      => EntryKind::Enter(p.clone()),
        }
    }

    fn label(&self) -> String {
        match self {
            Entry::SelectThis  => " \u{2190} Select this directory".into(),
            Entry::Parent      => " [..]".into(),
            Entry::Drive(p)    => format!(" [{}]", p.to_string_lossy()),
            Entry::Dir(p)      => format!(" [{}]",
                p.file_name().unwrap_or_default().to_string_lossy()),
        }
    }
}

// ---- Picker -----------------------------------------------------------------

enum Level { Drives, Dir(PathBuf) }

struct Picker {
    level:      Level,
    entries:    Vec<Entry>,
    selected:   usize,
    list_state: ListState,
}

impl Picker {
    fn new() -> Self {
        let mut p = Picker {
            level: Level::Drives,
            entries: Vec::new(),
            selected: 0,
            list_state: ListState::default(),
        };
        p.reload();
        p
    }

    fn reload(&mut self) {
        self.entries = match &self.level {
            Level::Drives => ('A'..='Z')
                .filter_map(|c| {
                    let p = PathBuf::from(format!("{}:\\", c));
                    if p.exists() { Some(Entry::Drive(p)) } else { None }
                })
                .collect(),

            Level::Dir(dir) => {
                let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|e| e.path())
                    .collect();
                dirs.sort();
                let mut v: Vec<Entry> = vec![Entry::SelectThis, Entry::Parent];
                v.extend(dirs.into_iter().map(Entry::Dir));
                v
            }
        };
        self.selected = 0;
        self.list_state.select(Some(0));
    }

    fn navigate(&mut self, delta: i32) {
        if self.entries.is_empty() { return; }
        let n = self.entries.len() as i32;
        self.selected = ((self.selected as i32 + delta).rem_euclid(n)) as usize;
        self.list_state.select(Some(self.selected));
    }

    fn go_up(&mut self) {
        if let Level::Dir(dir) = &self.level {
            self.level = match dir.parent() {
                Some(p) if !p.as_os_str().is_empty() => Level::Dir(p.to_path_buf()),
                _ => Level::Drives,
            };
        }
        self.reload();
    }

    fn activate(&mut self) -> Option<PathBuf> {
        let kind = self.entries.get(self.selected)?.kind();
        match kind {
            EntryKind::SelectThis => {
                if let Level::Dir(d) = &self.level { Some(d.clone()) } else { None }
            }
            EntryKind::GoUp => { self.go_up(); None }
            EntryKind::Enter(p) => {
                self.level = Level::Dir(p);
                self.reload();
                None
            }
        }
    }

    fn select_current(&self) -> Option<PathBuf> {
        if let Level::Dir(d) = &self.level { Some(d.clone()) } else { None }
    }

    fn header(&self) -> String {
        match &self.level {
            Level::Drives  => "Computer".into(),
            Level::Dir(d)  => d.to_string_lossy().into_owned(),
        }
    }
}

// ---- Mount input ------------------------------------------------------------

struct MountInput {
    game_dir: String,
    value:    String,
}

// ---- State machine ----------------------------------------------------------

enum State {
    Picker(Picker),
    Mount(MountInput),
}

// ---- Public entry point -----------------------------------------------------

pub fn run() -> Option<(String, String)> {
    terminal::enable_raw_mode().ok()?;
    let mut stdout = io::stdout();
    ratatui::crossterm::execute!(stdout, terminal::EnterAlternateScreen).ok();

    let backend  = CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend).ok()?;

    let result = event_loop(&mut term);

    ratatui::crossterm::execute!(io::stdout(), terminal::LeaveAlternateScreen).ok();
    terminal::disable_raw_mode().ok();
    result
}

fn event_loop(term: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Option<(String, String)> {
    let mut state = State::Picker(Picker::new());

    loop {
        term.draw(|f| draw(f, &mut state)).ok();

        if !event::poll(Duration::from_millis(250)).unwrap_or(false) { continue; }
        let ev = match event::read() { Ok(e) => e, Err(_) => continue };

        let Event::Key(key) = ev else { continue };

        // Collect any state transition outside the borrow so we can reassign.
        let mut next: Option<State> = None;
        let mut done: Option<(String, String)> = None;

        match &mut state {
            State::Picker(picker) => match key.code {
                KeyCode::Up       => picker.navigate(-1),
                KeyCode::Down     => picker.navigate(1),
                KeyCode::Backspace => picker.go_up(),
                KeyCode::Esc | KeyCode::Char('q')
                    if key.modifiers == KeyModifiers::NONE => return None,

                // Enter or Right: open subdir; if SelectThis → advance to mount step
                KeyCode::Enter | KeyCode::Right => {
                    if let Some(dir) = picker.activate() {
                        next = Some(State::Mount(MountInput {
                            game_dir: dir.to_string_lossy().into_owned(),
                            value:    String::new(),
                        }));
                    }
                }

                // Tab: select current directory without descending
                KeyCode::Tab => {
                    if let Some(dir) = picker.select_current() {
                        next = Some(State::Mount(MountInput {
                            game_dir: dir.to_string_lossy().into_owned(),
                            value:    String::new(),
                        }));
                    }
                }

                _ => {}
            },

            State::Mount(m) => match key.code {
                KeyCode::Esc => {
                    next = Some(State::Picker(Picker::new()));
                }
                KeyCode::Backspace => {
                    if !m.value.is_empty() {
                        m.value.pop();
                    } else {
                        next = Some(State::Picker(Picker::new()));
                    }
                }
                KeyCode::Char(c) => m.value.push(c),
                KeyCode::Enter => {
                    if !m.value.is_empty() {
                        done = Some((m.game_dir.clone(), m.value.clone()));
                    }
                }
                _ => {}
            },
        }

        if let Some(r) = done  { return Some(r); }
        if let Some(s) = next  { state = s; }
    }
}

// ---- Drawing ----------------------------------------------------------------

fn draw(f: &mut ratatui::Frame, state: &mut State) {
    let area = f.area();

    match state {
        State::Picker(picker) => draw_picker(f, area, picker),
        State::Mount(m)       => draw_mount(f, area, m),
    }
}

fn draw_picker(f: &mut ratatui::Frame, area: ratatui::layout::Rect, picker: &mut Picker) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    // Header: current path
    let hdr_text = picker.header();
    let header = Paragraph::new(Line::from(vec![
        Span::raw(" "),
        Span::styled(hdr_text.as_str(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    ]))
    .block(Block::default().borders(Borders::ALL).title(" Game directory "));
    f.render_widget(header, chunks[0]);

    // Entry list
    let items: Vec<ListItem> = picker.entries.iter().enumerate().map(|(i, e)| {
        let label = e.label();
        let style = if i == picker.selected {
            match e {
                Entry::SelectThis => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                _                 => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            }
        } else {
            match e {
                Entry::SelectThis => Style::default().fg(Color::Green),
                Entry::Parent     => Style::default().fg(Color::DarkGray),
                _                 => Style::default(),
            }
        };
        ListItem::new(Line::from(Span::styled(label, style)))
    }).collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, chunks[1], &mut picker.list_state);

    // Footer
    let footer = Paragraph::new(Line::from(vec![
        Span::raw("  "),
        Span::styled("\u{2191}\u{2193}", Style::default().fg(Color::Cyan)),
        Span::raw(" navigate  "),
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::raw(" open  "),
        Span::styled("Tab", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw(" select this dir  "),
        Span::styled("Backspace", Style::default().fg(Color::Cyan)),
        Span::raw(" up  "),
        Span::styled("Esc", Style::default().fg(Color::Red)),
        Span::raw(" cancel"),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}

fn draw_mount(f: &mut ratatui::Frame, area: ratatui::layout::Rect, m: &MountInput) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    let body = Paragraph::new(vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  Game directory: "),
            Span::styled(&m.game_dir, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  Mount point (e.g. "),
            Span::styled("Y:", Style::default().fg(Color::Yellow)),
            Span::raw("): "),
            Span::styled(&m.value, Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("_", Style::default().fg(Color::DarkGray)),
        ]),
    ])
    .block(Block::default().borders(Borders::ALL).title(" Mount point "));
    f.render_widget(body, chunks[0]);

    let footer = Paragraph::new(Line::from(vec![
        Span::raw("  "),
        Span::styled("Enter", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw(" confirm  "),
        Span::styled("Backspace", Style::default().fg(Color::Cyan)),
        Span::raw(" back  "),
        Span::styled("Esc", Style::default().fg(Color::Red)),
        Span::raw(" back"),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}
