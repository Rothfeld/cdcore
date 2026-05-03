//! First-run setup TUI: directory browser → mount-point input.
//!
//! Runs before the main filesystem is mounted; returns (game_dir, mount)
//! or None if the user pressed Esc.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::{
    backend::CrosstermBackend,
    crossterm::{event::{self, Event, KeyCode, KeyEventKind, KeyModifiers}, terminal},
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};

// ---- Detection --------------------------------------------------------------

/// Try to find the Crimson Desert packages directory automatically.
/// Returns the directory that directly contains 0000/, meta/, etc.
pub fn detect_game_dir() -> Option<PathBuf> {
    // 1. Registry: standard uninstall entries (runs a fast PowerShell one-liner)
    if let Some(p) = from_uninstall_registry() {
        return Some(p);
    }

    // 2. Common paths on every present drive
    for c in 'A'..='Z' {
        let drive = PathBuf::from(format!("{c}:\\"));
        if !drive.exists() { continue; }

        // The packages dir may be at the drive root directly
        if is_packages_dir(&drive) { return Some(drive.clone()); }

        // Or in a named subdirectory
        for sub in ["Pearl Abyss\\Crimson Desert",
                    "Pearl Abyss\\CrimsonDesert",
                    "Crimson Desert",
                    "CrimsonDesert"] {
            if let Some(p) = resolve_packages_dir(&drive.join(sub)) {
                return Some(p);
            }
        }
    }

    // 3. Program Files
    for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(pf) = std::env::var(var) {
            for sub in ["Pearl Abyss\\Crimson Desert", "Crimson Desert"] {
                if let Some(p) = resolve_packages_dir(&PathBuf::from(&pf).join(sub)) {
                    return Some(p);
                }
            }
        }
    }

    None
}

/// Spawn a PowerShell one-liner to enumerate uninstall registry keys and
/// find an entry whose DisplayName contains "Crimson Desert".
fn from_uninstall_registry() -> Option<PathBuf> {
    let ps = r#"
$keys = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*'
foreach ($k in $keys) {
    $hit = Get-ItemProperty $k -ErrorAction SilentlyContinue |
           Where-Object { $_.DisplayName -like '*Crimson Desert*' } |
           Select-Object -First 1
    if ($hit -and $hit.InstallLocation) { $hit.InstallLocation.TrimEnd('\'); break }
}
"#;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", ps])
        .output().ok()?;
    let raw = String::from_utf8_lossy(&out.stdout);
    let s = raw.trim();
    if s.is_empty() { return None; }
    resolve_packages_dir(&PathBuf::from(s))
}

/// Given a candidate root, check root itself and root\packages for the
/// packages structure (contains a meta\ subdirectory).
fn resolve_packages_dir(root: &Path) -> Option<PathBuf> {
    if is_packages_dir(root)                 { return Some(root.to_path_buf()); }
    let sub = root.join("packages");
    if is_packages_dir(&sub)                 { return Some(sub); }
    None
}

/// A directory is a valid packages dir if it contains a meta\ subdirectory.
fn is_packages_dir(dir: &Path) -> bool {
    dir.join("meta").is_dir()
}

/// Return the first drive letter (Z → D) that is not currently mounted.
pub fn detect_free_drive() -> Option<String> {
    for c in ('D'..='Z').rev() {
        if !PathBuf::from(format!("{c}:\\")).exists() {
            return Some(format!("{c}:"));
        }
    }
    None
}

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
    detected:   bool,   // whether game_dir was auto-detected (shown in header)
}

impl Picker {
    fn new_at(start: Option<PathBuf>) -> Self {
        let detected = start.is_some();
        let mut p = Picker {
            level: start.map(Level::Dir).unwrap_or(Level::Drives),
            entries: Vec::new(),
            selected: 0,
            list_state: ListState::default(),
            detected,
        };
        p.reload();
        p
    }

    fn reload(&mut self) {
        self.entries = match &self.level {
            Level::Drives => ('A'..='Z')
                .filter_map(|c| {
                    let p = PathBuf::from(format!("{c}:\\"));
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
        self.detected = false;
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
                self.detected = false;
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
    game_dir:  String,
    value:     String,
    detected:  bool,  // whether drive was auto-detected
}

// ---- State machine ----------------------------------------------------------

enum State {
    Picker(Picker),
    Mount(MountInput),
}

// ---- Public entry point -----------------------------------------------------

pub fn run() -> Option<(String, String)> {
    // Run detection before taking over the terminal so there's no flicker.
    let game_dir_hint  = detect_game_dir();
    let drive_hint     = detect_free_drive().unwrap_or_else(|| "Y:".to_string());

    terminal::enable_raw_mode().ok()?;
    let mut stdout = io::stdout();
    ratatui::crossterm::execute!(stdout, terminal::EnterAlternateScreen).ok();

    let backend  = CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend).ok()?;

    let result = event_loop(&mut term, game_dir_hint, drive_hint);

    ratatui::crossterm::execute!(io::stdout(), terminal::LeaveAlternateScreen).ok();
    terminal::disable_raw_mode().ok();
    result
}

fn event_loop(
    term:          &mut Terminal<CrosstermBackend<io::Stdout>>,
    game_dir_hint: Option<PathBuf>,
    drive_hint:    String,
) -> Option<(String, String)> {
    let detected_drive = detect_free_drive().is_some();
    let mut state = State::Picker(Picker::new_at(game_dir_hint));

    loop {
        term.draw(|f| draw(f, &mut state)).ok();

        if !event::poll(Duration::from_millis(250)).unwrap_or(false) { continue; }
        let ev = match event::read() { Ok(e) => e, Err(_) => continue };

        let Event::Key(key) = ev else { continue };
        if key.kind != KeyEventKind::Press { continue; }

        let mut next: Option<State> = None;
        let mut done: Option<(String, String)> = None;

        match &mut state {
            State::Picker(picker) => match key.code {
                KeyCode::Up       => picker.navigate(-1),
                KeyCode::Down     => picker.navigate(1),
                KeyCode::Backspace => picker.go_up(),
                KeyCode::Esc | KeyCode::Char('q')
                    if key.modifiers == KeyModifiers::NONE => return None,

                KeyCode::Enter | KeyCode::Right => {
                    if let Some(dir) = picker.activate() {
                        next = Some(State::Mount(MountInput {
                            game_dir: dir.to_string_lossy().into_owned(),
                            value:    drive_hint.clone(),
                            detected: detected_drive,
                        }));
                    }
                }

                KeyCode::Tab => {
                    if let Some(dir) = picker.select_current() {
                        next = Some(State::Mount(MountInput {
                            game_dir: dir.to_string_lossy().into_owned(),
                            value:    drive_hint.clone(),
                            detected: detected_drive,
                        }));
                    }
                }

                _ => {}
            },

            State::Mount(m) => match key.code {
                KeyCode::Esc => {
                    next = Some(State::Picker(Picker::new_at(None)));
                }
                KeyCode::Backspace => {
                    if !m.value.is_empty() {
                        m.value.pop();
                        m.detected = false;
                    } else {
                        next = Some(State::Picker(Picker::new_at(None)));
                    }
                }
                KeyCode::Char(c) => { m.value.push(c); m.detected = false; }
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
    match state {
        State::Picker(picker) => draw_picker(f, f.area(), picker),
        State::Mount(m)       => draw_mount(f, f.area(), m),
    }
}

fn draw_picker(f: &mut ratatui::Frame, area: ratatui::layout::Rect, picker: &mut Picker) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    let hdr_text  = picker.header();
    let det_label = if picker.detected { "  (detected)" } else { "" };
    let header = Paragraph::new(Line::from(vec![
        Span::raw(" "),
        Span::styled(hdr_text.as_str(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(det_label, Style::default().fg(Color::Green)),
    ]))
    .block(Block::default().borders(Borders::ALL).title(" Game directory "));
    f.render_widget(header, chunks[0]);

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

    let det_label = if m.detected { " (detected)" } else { "" };
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
            Span::styled(det_label, Style::default().fg(Color::Green)),
        ]),
    ])
    .block(Block::default().borders(Borders::ALL).title(" Mount point "));
    f.render_widget(body, chunks[0]);

    let footer = Paragraph::new(Line::from(vec![
        Span::raw("  "),
        Span::styled("Enter", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw(" confirm  "),
        Span::styled("Backspace", Style::default().fg(Color::Cyan)),
        Span::raw(" edit / back  "),
        Span::styled("Esc", Style::default().fg(Color::Red)),
        Span::raw(" back"),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}
