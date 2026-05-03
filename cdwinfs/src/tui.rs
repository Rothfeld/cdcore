use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
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
// Inline file picker (used inside the pre-mount config view)
// ============================================================================

enum EntryKind { SelectThis, GoUp, Enter(PathBuf) }

enum PickerEntry { SelectThis, Parent, Drive(PathBuf), Dir(PathBuf) }

impl PickerEntry {
    fn kind(&self) -> EntryKind {
        match self {
            PickerEntry::SelectThis  => EntryKind::SelectThis,
            PickerEntry::Parent      => EntryKind::GoUp,
            PickerEntry::Drive(p) | PickerEntry::Dir(p) => EntryKind::Enter(p.clone()),
        }
    }
    fn label(&self) -> String {
        match self {
            PickerEntry::SelectThis  => " \u{2190} Select this directory".into(),
            PickerEntry::Parent      => " [..]".into(),
            PickerEntry::Drive(p)    => format!(" [{}]", p.to_string_lossy()),
            PickerEntry::Dir(p)      => format!(" [{}]",
                p.file_name().unwrap_or_default().to_string_lossy()),
        }
    }
}

enum PickerLevel { Drives, Dir(PathBuf) }

pub struct Picker {
    level:      PickerLevel,
    entries:    Vec<PickerEntry>,
    selected:   usize,
    list_state: ListState,
}

impl Picker {
    pub fn new_at(start: Option<PathBuf>) -> Self {
        let mut p = Picker {
            level: start.map(PickerLevel::Dir).unwrap_or(PickerLevel::Drives),
            entries: Vec::new(),
            selected: 0,
            list_state: ListState::default(),
        };
        p.reload();
        p
    }

    fn reload(&mut self) {
        self.entries = match &self.level {
            PickerLevel::Drives => ('A'..='Z')
                .filter_map(|c| {
                    let p = PathBuf::from(format!("{c}:\\"));
                    if p.exists() { Some(PickerEntry::Drive(p)) } else { None }
                })
                .collect(),
            PickerLevel::Dir(dir) => {
                let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
                    .into_iter().flatten().flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|e| e.path())
                    .collect();
                dirs.sort();
                let mut v = vec![PickerEntry::SelectThis, PickerEntry::Parent];
                v.extend(dirs.into_iter().map(PickerEntry::Dir));
                v
            }
        };
        self.selected = 0;
        self.list_state.select(Some(0));
    }

    pub fn navigate(&mut self, delta: i32) {
        if self.entries.is_empty() { return; }
        let n = self.entries.len() as i32;
        self.selected = ((self.selected as i32 + delta).rem_euclid(n)) as usize;
        self.list_state.select(Some(self.selected));
    }

    pub fn go_up(&mut self) {
        if let PickerLevel::Dir(dir) = &self.level {
            self.level = match dir.parent() {
                Some(p) if !p.as_os_str().is_empty() => PickerLevel::Dir(p.to_path_buf()),
                _ => PickerLevel::Drives,
            };
        }
        self.reload();
    }

    /// Activate the selected entry.  Returns Some(path) if the user chose a dir.
    pub fn activate(&mut self) -> Option<PathBuf> {
        let kind = self.entries.get(self.selected)?.kind();
        match kind {
            EntryKind::SelectThis => {
                if let PickerLevel::Dir(d) = &self.level { Some(d.clone()) } else { None }
            }
            EntryKind::GoUp => { self.go_up(); None }
            EntryKind::Enter(p) => {
                self.level = PickerLevel::Dir(p);
                self.reload();
                None
            }
        }
    }

    pub fn select_current(&self) -> Option<PathBuf> {
        if let PickerLevel::Dir(d) = &self.level { Some(d.clone()) } else { None }
    }

    fn header(&self) -> String {
        match &self.level {
            PickerLevel::Drives  => "Computer".into(),
            PickerLevel::Dir(d)  => d.to_string_lossy().into_owned(),
        }
    }
}

// ============================================================================
// Pre-mount configuration view
// ============================================================================

enum ConfigBody {
    /// Showing the two path fields.
    Fields { editing_mount: bool },
    /// File picker is open for the game directory.
    Picker(Picker),
}

struct ConfigState {
    game_dir:          Option<String>,
    game_dir_detected: bool,
    mount:             String,
    mount_detected:    bool,
    body:              ConfigBody,
}

impl ConfigState {
    fn new(game_dir_hint: Option<PathBuf>, drive_hint: String, drive_detected: bool) -> Self {
        let (game_dir, game_dir_detected) = match &game_dir_hint {
            Some(p) => (Some(p.to_string_lossy().into_owned()), true),
            None    => (None, false),
        };
        // Normalise: accept "Y", "Y:", "y:", etc. — store as single uppercase letter.
        let mount = drive_hint.chars()
            .find(|c| c.is_ascii_alphabetic())
            .map(|c| c.to_ascii_uppercase().to_string())
            .unwrap_or_default();
        ConfigState {
            game_dir,
            game_dir_detected,
            mount,
            mount_detected: drive_detected,
            body: ConfigBody::Fields { editing_mount: false },
        }
    }

    fn can_mount(&self) -> bool {
        self.game_dir.as_ref().is_some_and(|s| !s.is_empty())
            && self.mount.len() == 1
    }
}

/// Show the pre-mount configuration TUI.  Returns (game_dir, mount) or None on Esc.
pub fn select_paths(
    game_dir_hint:  Option<PathBuf>,
    drive_hint:     String,
    drive_detected: bool,
) -> Option<(String, String)> {
    terminal::enable_raw_mode().ok()?;
    let mut stdout = io::stdout();
    ratatui::crossterm::execute!(stdout, terminal::EnterAlternateScreen).ok();

    let backend  = CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend).ok()?;

    let result = config_loop(&mut term, game_dir_hint, drive_hint, drive_detected);

    ratatui::crossterm::execute!(io::stdout(), terminal::LeaveAlternateScreen).ok();
    terminal::disable_raw_mode().ok();
    result
}

fn config_loop(
    term:           &mut Terminal<CrosstermBackend<io::Stdout>>,
    game_dir_hint:  Option<PathBuf>,
    drive_hint:     String,
    drive_detected: bool,
) -> Option<(String, String)> {
    let mut st = ConfigState::new(game_dir_hint.clone(), drive_hint, drive_detected);

    // Actions collected from the event handler to avoid borrow-checker conflicts
    // when reading st.body mutably and st.game_dir/mount immutably in the same arm.
    enum A {
        None,
        Quit,
        Mount,
        PickerSelect(PathBuf),
        PickerBack,
        OpenPicker,
        StartEditMount,
        MountChar(char),
        MountPop,
        MountDone,
    }

    loop {
        term.draw(|f| draw_config(f, &mut st)).ok();

        if !event::poll(Duration::from_millis(250)).unwrap_or(false) { continue; }
        let ev = match event::read() { Ok(e) => e, Err(_) => continue };
        let Event::Key(key) = ev else { continue };
        if key.kind != KeyEventKind::Press { continue; }

        let action = match &mut st.body {
            ConfigBody::Picker(picker) => match key.code {
                KeyCode::Up        => { picker.navigate(-1); A::None }
                KeyCode::Down      => { picker.navigate(1);  A::None }
                KeyCode::Backspace => { picker.go_up();      A::None }
                KeyCode::Enter | KeyCode::Right => {
                    picker.activate().map(A::PickerSelect).unwrap_or(A::None)
                }
                KeyCode::Tab => {
                    picker.select_current().map(A::PickerSelect).unwrap_or(A::None)
                }
                KeyCode::Esc => A::PickerBack,
                _ => A::None,
            },
            ConfigBody::Fields { editing_mount } => {
                if *editing_mount {
                    match key.code {
                        KeyCode::Char(c)               => A::MountChar(c),
                        KeyCode::Backspace              => A::MountPop,
                        KeyCode::Enter | KeyCode::Esc  => A::MountDone,
                        _                              => A::None,
                    }
                } else {
                    match (key.code, key.modifiers) {
                        (KeyCode::Esc, _) => A::Quit,
                        (KeyCode::Enter, _)                  => A::Mount,
                        (KeyCode::Char('g'), KeyModifiers::NONE) => A::OpenPicker,
                        (KeyCode::Char('m'), KeyModifiers::NONE) => A::StartEditMount,
                        _ => A::None,
                    }
                }
            }
        };

        match action {
            A::None => {}
            A::Quit => return None,
            A::Mount => {
                if st.can_mount() {
                    // Append colon: "Y" → "Y:" as expected by WinFSP.
                    return Some((st.game_dir.clone().unwrap(), format!("{}:", st.mount)));
                }
            }
            A::PickerSelect(dir) => {
                st.game_dir = Some(dir.to_string_lossy().into_owned());
                st.game_dir_detected = false;
                st.body = ConfigBody::Fields { editing_mount: false };
            }
            A::PickerBack => {
                st.body = ConfigBody::Fields { editing_mount: false };
            }
            A::OpenPicker => {
                let start = st.game_dir.as_ref().map(PathBuf::from)
                    .or_else(|| game_dir_hint.clone());
                st.body = ConfigBody::Picker(Picker::new_at(start));
            }
            A::StartEditMount => {
                st.body = ConfigBody::Fields { editing_mount: true };
            }
            A::MountChar(c) => {
                if c.is_ascii_alphabetic() {
                    // Single char, uppercase, auto-confirm: exit edit mode immediately.
                    st.mount = c.to_ascii_uppercase().to_string();
                    st.mount_detected = false;
                    st.body = ConfigBody::Fields { editing_mount: false };
                }
                // Non-alpha: ignore silently.
            }
            A::MountPop => {
                st.mount.clear();
                st.mount_detected = false;
                // Stay in edit mode so the user can type a new letter.
            }
            A::MountDone => {
                // Enter/Esc exits edit mode without requiring a letter (allows cancelling).
                st.body = ConfigBody::Fields { editing_mount: false };
            }
        }
    }
}

fn draw_config(f: &mut ratatui::Frame, st: &mut ConfigState) {
    let area = f.area();

    match &mut st.body {
        ConfigBody::Picker(picker) => {
            // File picker occupies the full area (same layout as fields, body replaced)
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
                .split(area);

            let hdr_text = picker.header();
            let header = Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(hdr_text.as_str(),
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
                } else {
                    base
                };
                ListItem::new(Line::from(Span::styled(label, style)))
            }).collect();

            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL));
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

            // Header
            let header = Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled("cdwinfs", Style::default().add_modifier(Modifier::BOLD)),
            ]))
            .block(Block::default().borders(Borders::ALL));
            f.render_widget(header, chunks[0]);

            // Body: two path fields
            let can_mount = st.can_mount();

            let (gd_text, gd_style, gd_hint) = match &st.game_dir {
                Some(s) if !s.is_empty() => {
                    let hint = if st.game_dir_detected { " (detected)" } else { "" };
                    (s.as_str(), Style::default().fg(Color::Cyan), hint)
                }
                _ => ("[not configured]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD), ""),
            };

            // Drive letter display.
            let drive_display;
            let (dl_prefix, mt_text, mt_style, mt_suffix): (&str, &str, Style, &str) =
                if editing_mount {
                    // Reversed highlight + explicit cursor makes the active field unmissable.
                    let style = Style::default()
                        .fg(Color::Black).bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD);
                    if st.mount.is_empty() {
                        ("\u{25b6} ", " A-Z ", style, "")
                    } else {
                        drive_display = format!(" {}:_ ", st.mount);
                        ("\u{25b6} ", drive_display.as_str(), style, "")
                    }
                } else if st.mount.is_empty() {
                    ("  ", "[no drive selected]",
                     Style::default().fg(Color::Red).add_modifier(Modifier::BOLD), "")
                } else {
                    drive_display = format!("{}:", st.mount);
                    let hint = if st.mount_detected { " (auto)" } else { "" };
                    ("  ", drive_display.as_str(), Style::default().fg(Color::Cyan), hint)
                };

            let body_title = if editing_mount {
                " Configuration — type A-Z "
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
                    Span::raw(dl_prefix),
                    Span::raw("Drive letter:    "),
                    Span::styled(mt_text, mt_style),
                    Span::styled(mt_suffix, Style::default().fg(Color::Green)),
                    Span::raw("   "),
                    Span::styled("[m] change", Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(""),
            ];

            let body = Paragraph::new(body_lines)
                .block(Block::default().borders(Borders::ALL).title(body_title));
            f.render_widget(body, chunks[1]);

            // Footer
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
                Span::raw(" drive letter   "),
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
// Normal post-mount TUI (pending writes + events)
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

// Need KeyEvent in scope for the pattern above
use ratatui::crossterm::event::KeyEvent;

fn draw(
    f:             &mut ratatui::Frame,
    mount:         &str,
    readonly:      bool,
    has_vgmstream: bool,
    has_ffmpeg:    bool,
    pending:       &[String],
    events:        &[String],
    saving:   bool,
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

    // Header
    let rw_label = if readonly { "  (ro)" } else { "  (rw)" };
    let rw_style = if readonly {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let tool_dim = Style::default().fg(Color::DarkGray);
    let header = Paragraph::new(Line::from(vec![
        Span::raw(" "),
        Span::styled("cdwinfs", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(mount, Style::default().fg(Color::Cyan)),
        Span::styled(rw_label, rw_style),
        Span::raw("  "),
        Span::styled("vgm:", tool_dim),
        Span::styled(if has_vgmstream { "✓" } else { "✗" },
            Style::default().fg(if has_vgmstream { Color::Green } else { Color::Red })),
        Span::raw(" "),
        Span::styled("ffmpeg:", tool_dim),
        Span::styled(if has_ffmpeg { "✓" } else { "✗" },
            Style::default().fg(if has_ffmpeg { Color::Green } else { Color::Red })),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(header, chunks[0]);

    // Body: pending writes
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

    // Events panel
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

    // Footer
    let s_style = if saving || pending.is_empty() || readonly {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    };
    let footer = Paragraph::new(Line::from(vec![
        Span::raw("  "),
        Span::styled("[s]", s_style),
        Span::raw(" save    "),
        Span::styled("Esc", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Span::raw(if pending.is_empty() { " quit" } else { " quit without saving" }),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[3]);
}
