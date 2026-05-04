use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};

use crate::fs::SharedFs;

// ============================================================================
// Inline directory picker
// ============================================================================

enum EntryKind { SelectThis, GoUp, Enter(PathBuf) }

enum PickerEntry { SelectThis, Parent, Dir(PathBuf) }

impl PickerEntry {
    fn kind(&self) -> EntryKind {
        match self {
            PickerEntry::SelectThis => EntryKind::SelectThis,
            PickerEntry::Parent     => EntryKind::GoUp,
            PickerEntry::Dir(p)     => EntryKind::Enter(p.clone()),
        }
    }
    fn label(&self) -> String {
        match self {
            PickerEntry::SelectThis => " \u{2190} Select this directory".into(),
            PickerEntry::Parent     => " [..]".into(),
            PickerEntry::Dir(p)     => format!(" [{}]",
                p.file_name().unwrap_or(p.as_os_str()).to_string_lossy()),
        }
    }
}

struct Picker {
    current:    PathBuf,
    entries:    Vec<PickerEntry>,
    selected:   usize,
    list_state: ListState,
}

impl Picker {
    fn new_at(start: Option<PathBuf>) -> Self {
        let current = start.unwrap_or_else(|| PathBuf::from("/"));
        let mut p = Picker { current, entries: Vec::new(), selected: 0,
                             list_state: ListState::default() };
        p.reload();
        p
    }

    fn reload(&mut self) {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&self.current)
            .into_iter().flatten().flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .collect();
        dirs.sort();
        let mut v = vec![PickerEntry::SelectThis, PickerEntry::Parent];
        v.extend(dirs.into_iter().map(PickerEntry::Dir));
        self.entries = v;
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
        if let Some(p) = self.current.parent() {
            self.current = p.to_path_buf();
            self.reload();
        }
    }

    fn activate(&mut self) -> Option<PathBuf> {
        let kind = self.entries.get(self.selected)?.kind();
        match kind {
            EntryKind::SelectThis  => Some(self.current.clone()),
            EntryKind::GoUp        => { self.go_up(); None }
            EntryKind::Enter(p)    => { self.current = p; self.reload(); None }
        }
    }

    fn select_current(&self) -> PathBuf { self.current.clone() }
}

// ============================================================================
// Pre-mount configuration view
// ============================================================================

enum ConfigBody {
    Fields { editing_mount: bool },
    Picking(Picker),
}

struct ConfigState {
    game_dir:          Option<String>,
    game_dir_detected: bool,
    mount:             String,
    body:              ConfigBody,
}

impl ConfigState {
    fn new(game_dir_hint: Option<PathBuf>, mount_hint: String) -> Self {
        let (game_dir, game_dir_detected) = match &game_dir_hint {
            Some(p) => (Some(p.to_string_lossy().into_owned()), true),
            None    => (None, false),
        };
        ConfigState {
            game_dir,
            game_dir_detected,
            mount: mount_hint,
            body: ConfigBody::Fields { editing_mount: false },
        }
    }

    fn can_mount(&self) -> bool {
        self.game_dir.as_ref().is_some_and(|s| !s.is_empty()) && !self.mount.is_empty()
    }
}

pub fn select_paths(game_dir_hint: Option<PathBuf>, mount_hint: String) -> Option<(String, String)> {
    terminal::enable_raw_mode().ok()?;
    let mut stdout = io::stdout();
    ratatui::crossterm::execute!(stdout, terminal::EnterAlternateScreen).ok();
    let backend  = CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend).ok()?;
    let result = config_loop(&mut term, game_dir_hint, mount_hint);
    ratatui::crossterm::execute!(io::stdout(), terminal::LeaveAlternateScreen).ok();
    terminal::disable_raw_mode().ok();
    result
}

fn config_loop(
    term:          &mut Terminal<CrosstermBackend<io::Stdout>>,
    game_dir_hint: Option<PathBuf>,
    mount_hint:    String,
) -> Option<(String, String)> {
    let mut st = ConfigState::new(game_dir_hint.clone(), mount_hint);

    enum A {
        None, Quit, Mount,
        PickerActivate, PickerTab, PickerBack,
        OpenPicker, StartEditMount,
        MountChar(char), MountPop, MountDone,
    }

    loop {
        term.draw(|f| draw_config(f, &mut st)).ok();

        if !event::poll(Duration::from_millis(250)).unwrap_or(false) { continue; }
        let ev = match event::read() { Ok(e) => e, Err(_) => continue };
        let Event::Key(key) = ev else { continue };
        if key.kind != KeyEventKind::Press { continue; }

        let action = match &mut st.body {
            ConfigBody::Picking(picker) => match key.code {
                KeyCode::Up        => { picker.navigate(-1); A::None }
                KeyCode::Down      => { picker.navigate(1);  A::None }
                KeyCode::Backspace => { picker.go_up();      A::None }
                KeyCode::Enter | KeyCode::Right => {
                    picker.activate().map(|_| A::PickerActivate).unwrap_or(A::None)
                }
                KeyCode::Tab  => A::PickerTab,
                KeyCode::Esc  => A::PickerBack,
                _ => A::None,
            },
            ConfigBody::Fields { editing_mount } => {
                if *editing_mount {
                    match key.code {
                        KeyCode::Char(c)              => A::MountChar(c),
                        KeyCode::Backspace             => A::MountPop,
                        KeyCode::Enter | KeyCode::Esc => A::MountDone,
                        _ => A::None,
                    }
                } else {
                    match (key.code, key.modifiers) {
                        (KeyCode::Esc, _) => A::Quit,
                        (KeyCode::Enter, _) => A::Mount,
                        (KeyCode::Char('g'), KeyModifiers::NONE) => A::OpenPicker,
                        (KeyCode::Char('m'), KeyModifiers::NONE) => A::StartEditMount,
                        _ => A::None,
                    }
                }
            }
        };

        // Resolve picker selection outside the mutable borrow.
        let picker_selection = if matches!(action, A::PickerActivate | A::PickerTab) {
            if let ConfigBody::Picking(p) = &st.body {
                Some(match action {
                    A::PickerActivate => p.select_current(),
                    _                 => p.select_current(),
                })
            } else { None }
        } else { None };

        match action {
            A::None => {}
            A::Quit => return None,
            A::Mount => {
                if st.can_mount() {
                    return Some((st.game_dir.clone().unwrap(), st.mount.clone()));
                }
            }
            A::PickerActivate | A::PickerTab => {
                if let Some(dir) = picker_selection {
                    st.game_dir = Some(dir.to_string_lossy().into_owned());
                    st.game_dir_detected = false;
                    st.body = ConfigBody::Fields { editing_mount: false };
                }
            }
            A::PickerBack => {
                st.body = ConfigBody::Fields { editing_mount: false };
            }
            A::OpenPicker => {
                let start = st.game_dir.as_ref().map(PathBuf::from)
                    .or_else(|| game_dir_hint.clone());
                st.body = ConfigBody::Picking(Picker::new_at(start));
            }
            A::StartEditMount => {
                st.body = ConfigBody::Fields { editing_mount: true };
            }
            A::MountChar(c) => { st.mount.push(c); }
            A::MountPop     => { st.mount.pop(); }
            A::MountDone    => { st.body = ConfigBody::Fields { editing_mount: false }; }
        }
    }
}

fn draw_config(f: &mut ratatui::Frame, st: &mut ConfigState) {
    let area = f.area();
    match &mut st.body {
        ConfigBody::Picking(picker) => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
                .split(area);

            let hdr = picker.current.to_string_lossy().into_owned();
            let header = Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(hdr.as_str(),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            ]))
            .block(Block::default().borders(Borders::ALL).title(" Game directory "));
            f.render_widget(header, chunks[0]);

            let items: Vec<ListItem> = picker.entries.iter().enumerate().map(|(i, e)| {
                let label = e.label();
                let base = match e {
                    PickerEntry::SelectThis => Style::default().fg(Color::Green),
                    PickerEntry::Parent     => Style::default().fg(Color::DarkGray),
                    _                       => Style::default(),
                };
                let style = if i == picker.selected {
                    base.add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else { base };
                ListItem::new(Line::from(Span::styled(label, style)))
            }).collect();
            let list = List::new(items).block(Block::default().borders(Borders::ALL));
            f.render_stateful_widget(list, chunks[1], &mut picker.list_state);

            let footer = Paragraph::new(Line::from(vec![
                Span::raw("  "),
                Span::styled("\u{2191}\u{2193}", Style::default().fg(Color::Cyan)),
                Span::raw(" navigate  "),
                Span::styled("Enter", Style::default().fg(Color::Yellow)),
                Span::raw(" open  "),
                Span::styled("Tab", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(" select this dir  "),
                Span::styled("Backspace", Style::default().fg(Color::Cyan)),
                Span::raw(" up  "),
                Span::styled("Esc", Style::default().fg(Color::Red)),
                Span::raw(" back"),
            ]))
            .block(Block::default().borders(Borders::ALL));
            f.render_widget(footer, chunks[2]);
        }

        ConfigBody::Fields { editing_mount } => {
            let editing_mount = *editing_mount;
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
                .split(area);

            let header = Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled("cdfuse", Style::default().add_modifier(Modifier::BOLD)),
            ]))
            .block(Block::default().borders(Borders::ALL));
            f.render_widget(header, chunks[0]);

            let can_mount = st.can_mount();

            let (gd_text, gd_style, gd_hint) = match &st.game_dir {
                Some(s) if !s.is_empty() => {
                    let hint = if st.game_dir_detected { " (detected)" } else { "" };
                    (s.as_str(), Style::default().fg(Color::Cyan), hint)
                }
                _ => ("[not configured]",
                      Style::default().fg(Color::Red).add_modifier(Modifier::BOLD), ""),
            };

            let mount_display = format!("{}{}", st.mount, if editing_mount { "_" } else { "" });
            let (mp_prefix, mp_text, mp_style): (&str, &str, Style) = if editing_mount {
                ("\u{25b6} ", mount_display.as_str(),
                 Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD))
            } else if st.mount.is_empty() {
                ("  ", "[not configured]",
                 Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
            } else {
                ("  ", st.mount.as_str(), Style::default().fg(Color::Cyan))
            };

            let body_title = if editing_mount {
                " Configuration — type mount path "
            } else {
                " Configuration "
            };

            let body_lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::raw("  Game directory:  "),
                    Span::styled(gd_text, gd_style),
                    Span::styled(gd_hint, Style::default().fg(Color::Green)),
                    Span::raw("   "),
                    Span::styled("[g] browse", Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::raw(mp_prefix),
                    Span::raw("Mount point:     "),
                    Span::styled(mp_text, mp_style),
                    Span::raw("   "),
                    Span::styled("[m] edit", Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(""),
            ];
            let body = Paragraph::new(body_lines)
                .block(Block::default().borders(Borders::ALL).title(body_title));
            f.render_widget(body, chunks[1]);

            let enter_style = if can_mount {
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let footer = Paragraph::new(Line::from(vec![
                Span::raw("  "),
                Span::styled("[g]", Style::default().fg(Color::Cyan)),
                Span::raw(" browse game dir   "),
                Span::styled("[m]", Style::default().fg(Color::Cyan)),
                Span::raw(" mount point   "),
                Span::styled("Enter", enter_style),
                Span::raw(" mount   "),
                Span::styled("Esc", Style::default().fg(Color::Red)),
                Span::raw(" quit"),
            ]))
            .block(Block::default().borders(Borders::ALL));
            f.render_widget(footer, chunks[2]);
        }
    }
}

// ============================================================================
// Post-mount TUI (pending writes + events)
// ============================================================================

pub enum Action { Abort }

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
        let events    = shared.recent_events();
        let is_saving = saving.load(Ordering::Relaxed);
        term.draw(|f| draw(f, mount, shared.is_readonly(),
                           shared.has_vgmstream(), shared.has_ffmpeg(),
                           &pending, &events, is_saving)).ok();

        if event::poll(Duration::from_millis(250)).unwrap_or(false) {
            if let Ok(Event::Key(KeyEvent { code, modifiers, .. })) = event::read() {
                match (code, modifiers) {
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

                    (KeyCode::Esc, _)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Action::Abort,

                    _ => {}
                }
            }
        }
    }
}

fn draw(
    f: &mut ratatui::Frame,
    mount: &str,
    readonly: bool,
    has_vgmstream: bool,
    has_ffmpeg: bool,
    pending: &[String],
    events: &[String],
    saving: bool,
) {
    let area = f.area();
    let event_height = if events.is_empty() { 0 } else { (events.len() as u16 + 2).min(7) };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(event_height),
            Constraint::Length(3),
        ])
        .split(area);

    // -- Header ----------------------------------------------------------------
    let rw_label = if readonly { "  (ro)" } else { "  (rw)" };
    let rw_style = if readonly {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let header = Paragraph::new(Line::from(vec![
        Span::raw(" "),
        Span::styled("cdfuse", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(mount, Style::default().fg(Color::Cyan)),
        Span::styled(rw_label, rw_style),
        Span::raw("  "),
        Span::styled("wmmogg",
            Style::default().fg(if has_vgmstream { Color::Green } else { Color::Red })),
        Span::raw(" "),
        Span::styled("ffmpeg",
            Style::default().fg(if has_ffmpeg { Color::Green } else { Color::Red })),
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

    // -- Events ----------------------------------------------------------------
    if event_height > 0 {
        let rows: Vec<ListItem> = events.iter().map(|e| {
            let style = if e.starts_with("[err]") {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Green)
            };
            ListItem::new(Line::from(vec![Span::raw(" "), Span::styled(e, style)]))
        }).collect();
        let evlist = List::new(rows)
            .block(Block::default().borders(Borders::ALL).title(" Events "));
        f.render_widget(evlist, chunks[2]);
    }

    // -- Footer ----------------------------------------------------------------
    let s_style = if saving || pending.is_empty() || readonly {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    };
    let spans = vec![
        Span::raw("  "),
        Span::styled("[s]", s_style),
        Span::raw(" save    "),
        Span::styled("Esc", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Span::raw(if pending.is_empty() { " quit" } else { " quit without saving" }),
    ];

    let footer = Paragraph::new(Line::from(spans))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[3]);
}
