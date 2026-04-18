//! cargo-wipe: A TUI tool for finding and reclaiming disk space from Cargo
//! `target/` directories.
//!
//! Usage:
//!   cargo-wipe [START_DIR]
//!
//! If `START_DIR` is omitted, the scan starts at `$HOME` (falling back to the
//! current directory if `$HOME` is unset).

use std::{
    fs,
    io::{self, Stdout},
    panic,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use bytesize::ByteSize;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// A Cargo workspace (or standalone package) with a reclaimable `target/` dir.
#[derive(Debug, Clone)]
struct Workspace {
    root: PathBuf,
    target_path: PathBuf,
    target_size: u64,
    selected: bool,
}

/// A message sent from a background scan thread to the TUI.
enum ScanMsg {
    /// A chunk of workspaces has been fully scanned and sized.
    Batch(Vec<Workspace>),
    /// The scan finished.
    Done,
    /// A fatal scanning error.
    Error(String),
}

// ---------------------------------------------------------------------------
// scan module
// ---------------------------------------------------------------------------

mod scan {
    use super::*;
    use rayon::prelude::*;
    use walkdir::WalkDir;

    /// Returns true if the file name corresponds to a directory we don't want
    /// to descend into during the workspace-discovery walk.
    fn is_skippable_dir(name: &str) -> bool {
        // Skip build artifacts, VCS directories, hidden dirs, and some common
        // cache directories that can be huge and never contain Cargo projects
        // we care about.
        matches!(
            name,
            "target" | ".git" | "node_modules" | ".svn" | ".hg"
        ) || (name.starts_with('.') && name != "." && name != "..")
    }

    /// Reads a `Cargo.toml` and returns `(has_workspace_section, has_package_section)`.
    ///
    /// This is a deliberately lightweight parser: full TOML parsing would pull
    /// in a heavy dependency we don't otherwise need. Top-level table headers
    /// (`[workspace]`, `[package]`) always appear on their own line, so a
    /// line-by-line scan is accurate enough for the root-detection heuristic.
    fn inspect_cargo_toml(path: &Path) -> Result<(bool, bool)> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut has_workspace = false;
        let mut has_package = false;
        for raw in content.lines() {
            // Strip inline comments before trimming so `[package] # stuff` works.
            let line = match raw.find('#') {
                Some(i) => &raw[..i],
                None => raw,
            };
            let line = line.trim();
            if line == "[workspace]" || line.starts_with("[workspace.") {
                has_workspace = true;
            } else if line == "[package]" {
                has_package = true;
            }
        }
        Ok((has_workspace, has_package))
    }

    /// Recursively sums the sizes of all regular files under `dir`.
    fn dir_size(dir: &Path) -> u64 {
        WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum()
    }

    /// A candidate discovered during the walk.
    #[derive(Debug, Clone)]
    struct Candidate {
        root: PathBuf,
        has_workspace: bool,
        has_package: bool,
    }

    /// Determine whether `candidate` is a true root:
    /// - it declares `[workspace]`, or
    /// - it declares `[package]` and no ancestor (within the scanned set)
    ///   claims it as a workspace member.
    fn is_root(candidate: &Candidate, all: &[Candidate]) -> bool {
        if candidate.has_workspace {
            return true;
        }
        if !candidate.has_package {
            return false;
        }
        for ancestor in candidate.root.ancestors().skip(1) {
            if let Some(found) = all.iter().find(|c| c.root == ancestor) {
                if found.has_workspace {
                    return false;
                }
            }
        }
        true
    }

    /// Scan `start` for Cargo workspaces / standalone packages and return
    /// those that have a non-empty `target/` dir, sorted by target size
    /// descending.
    pub fn scan(start: &Path) -> Result<Vec<Workspace>> {
        if !start.exists() {
            return Err(anyhow!("scan root does not exist: {}", start.display()));
        }

        // Phase 1: discover every Cargo.toml under the start dir, skipping the
        // directories we never want to descend into.
        let mut candidates: Vec<Candidate> = Vec::new();
        let walker = WalkDir::new(start)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                if !e.file_type().is_dir() {
                    return true;
                }
                match e.file_name().to_str() {
                    Some(name) => !is_skippable_dir(name),
                    None => false,
                }
            });

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                // Skip unreadable entries silently rather than aborting.
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.file_name() != "Cargo.toml" {
                continue;
            }
            let path = entry.path();
            let root = match path.parent() {
                Some(p) => p.to_path_buf(),
                None => continue,
            };
            let (has_workspace, has_package) = match inspect_cargo_toml(path) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if !has_workspace && !has_package {
                continue;
            }
            candidates.push(Candidate {
                root,
                has_workspace,
                has_package,
            });
        }

        // Phase 2: filter to roots.
        let roots: Vec<Candidate> = candidates
            .iter()
            .filter(|c| is_root(c, &candidates))
            .cloned()
            .collect();

        // Phase 3: compute target sizes in parallel.
        let mut workspaces: Vec<Workspace> = roots
            .par_iter()
            .filter_map(|c| {
                let target = c.root.join("target");
                if !target.is_dir() {
                    return None;
                }
                let size = dir_size(&target);
                if size == 0 {
                    return None;
                }
                Some(Workspace {
                    root: c.root.clone(),
                    target_path: target,
                    target_size: size,
                    selected: false,
                })
            })
            .collect();

        workspaces.sort_by(|a, b| b.target_size.cmp(&a.target_size));
        Ok(workspaces)
    }

    /// Run `scan` in a dedicated thread, returning a receiver that produces
    /// progress messages.
    pub fn spawn(start: PathBuf) -> (JoinHandle<()>, Receiver<ScanMsg>) {
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || match scan(&start) {
            Ok(ws) => {
                let _ = tx.send(ScanMsg::Batch(ws));
                let _ = tx.send(ScanMsg::Done);
            }
            Err(e) => {
                let _ = tx.send(ScanMsg::Error(format!("{:#}", e)));
                let _ = tx.send(ScanMsg::Done);
            }
        });
        (handle, rx)
    }
}

// ---------------------------------------------------------------------------
// Deletion
// ---------------------------------------------------------------------------

/// Result of a delete attempt on one workspace.
#[derive(Debug, Clone)]
struct DeleteOutcome {
    root: PathBuf,
    freed: u64,
    error: Option<String>,
}

fn delete_target(ws: &Workspace) -> Result<u64> {
    // Only delete a path that is non-empty, has a parent, and is named "target".
    if ws.target_path.as_os_str().is_empty() {
        return Err(anyhow!("empty target path"));
    }
    if ws.target_path.parent().is_none() {
        return Err(anyhow!(
            "refusing to delete {} (no parent)",
            ws.target_path.display()
        ));
    }
    match ws.target_path.file_name().and_then(|n| n.to_str()) {
        Some("target") => {}
        _ => {
            return Err(anyhow!(
                "refusing to delete non-target path {}",
                ws.target_path.display()
            ));
        }
    }
    fs::remove_dir_all(&ws.target_path)
        .with_context(|| format!("removing {}", ws.target_path.display()))?;
    Ok(ws.target_size)
}

// ---------------------------------------------------------------------------
// TUI application state
// ---------------------------------------------------------------------------

/// High-level mode of the app.
#[derive(Debug, Clone)]
enum Mode {
    /// Busy scanning; show a spinner.
    Scanning,
    /// Viewing the list of workspaces.
    Browsing,
    /// Confirming a deletion.
    Confirm,
    /// Showing the results of a deletion pass.
    Results(Vec<DeleteOutcome>),
}

struct App {
    start_dir: PathBuf,
    mode: Mode,
    workspaces: Vec<Workspace>,
    list_state: ListState,
    status: Option<String>,
    status_is_error: bool,
    // Scan plumbing.
    scan_rx: Option<Receiver<ScanMsg>>,
    scan_handle: Option<JoinHandle<()>>,
    spinner_frame: usize,
    last_tick: Instant,
}

impl App {
    fn new(start_dir: PathBuf) -> Self {
        let (handle, rx) = scan::spawn(start_dir.clone());
        let status = Some(format!("Scanning {}...", start_dir.display()));
        Self {
            start_dir,
            mode: Mode::Scanning,
            workspaces: Vec::new(),
            list_state: ListState::default(),
            status,
            status_is_error: false,
            scan_rx: Some(rx),
            scan_handle: Some(handle),
            spinner_frame: 0,
            last_tick: Instant::now(),
        }
    }

    fn begin_rescan(&mut self) {
        // Drop previous handle; the thread will finish on its own.
        self.scan_handle.take();
        let (handle, rx) = scan::spawn(self.start_dir.clone());
        self.scan_rx = Some(rx);
        self.scan_handle = Some(handle);
        self.mode = Mode::Scanning;
        self.workspaces.clear();
        self.list_state.select(None);
        self.status = Some(format!("Rescanning {}...", self.start_dir.display()));
        self.status_is_error = false;
    }

    fn selected_count(&self) -> usize {
        self.workspaces.iter().filter(|w| w.selected).count()
    }

    fn selected_size(&self) -> u64 {
        self.workspaces
            .iter()
            .filter(|w| w.selected)
            .map(|w| w.target_size)
            .sum()
    }

    fn total_size(&self) -> u64 {
        self.workspaces.iter().map(|w| w.target_size).sum()
    }

    fn toggle_current(&mut self) {
        if let Some(i) = self.list_state.selected() {
            if let Some(ws) = self.workspaces.get_mut(i) {
                ws.selected = !ws.selected;
            }
        }
    }

    fn select_all(&mut self) {
        let any_unselected = self.workspaces.iter().any(|w| !w.selected);
        for w in &mut self.workspaces {
            w.selected = any_unselected;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.workspaces.is_empty() {
            self.list_state.select(None);
            return;
        }
        let len = self.workspaces.len() as isize;
        let current = self.list_state.selected().unwrap_or(0) as isize;
        let mut next = current + delta;
        if next < 0 {
            next = 0;
        }
        if next >= len {
            next = len - 1;
        }
        self.list_state.select(Some(next as usize));
    }

    fn jump_to(&mut self, pos: Position) {
        if self.workspaces.is_empty() {
            self.list_state.select(None);
            return;
        }
        match pos {
            Position::First => self.list_state.select(Some(0)),
            Position::Last => self.list_state.select(Some(self.workspaces.len() - 1)),
        }
    }

    /// Poll for scan messages. Returns true if the mode/data changed.
    fn poll_scan(&mut self) -> bool {
        let mut changed = false;
        let mut done = false;
        if let Some(rx) = &self.scan_rx {
            loop {
                match rx.try_recv() {
                    Ok(ScanMsg::Batch(ws)) => {
                        self.workspaces.extend(ws);
                        self.workspaces
                            .sort_by(|a, b| b.target_size.cmp(&a.target_size));
                        changed = true;
                    }
                    Ok(ScanMsg::Done) => {
                        done = true;
                        break;
                    }
                    Ok(ScanMsg::Error(msg)) => {
                        self.status = Some(format!("Scan error: {}", msg));
                        self.status_is_error = true;
                        changed = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        done = true;
                        break;
                    }
                }
            }
        }
        if done {
            self.scan_rx = None;
            self.scan_handle.take();
            self.mode = Mode::Browsing;
            if !self.status_is_error {
                if self.workspaces.is_empty() {
                    self.status = Some(format!(
                        "No reclaimable target/ directories under {}",
                        self.start_dir.display()
                    ));
                } else {
                    self.status = Some(format!(
                        "Found {} workspaces ({} total)",
                        self.workspaces.len(),
                        ByteSize::b(self.total_size())
                    ));
                }
            }
            if !self.workspaces.is_empty() && self.list_state.selected().is_none() {
                self.list_state.select(Some(0));
            }
            changed = true;
        }
        changed
    }

    /// Execute deletion of all currently selected workspaces.
    fn run_delete(&mut self) {
        let mut outcomes: Vec<DeleteOutcome> = Vec::new();
        let mut to_remove: Vec<usize> = Vec::new();
        for (i, ws) in self.workspaces.iter().enumerate() {
            if !ws.selected {
                continue;
            }
            match delete_target(ws) {
                Ok(freed) => {
                    outcomes.push(DeleteOutcome {
                        root: ws.root.clone(),
                        freed,
                        error: None,
                    });
                    to_remove.push(i);
                }
                Err(e) => {
                    outcomes.push(DeleteOutcome {
                        root: ws.root.clone(),
                        freed: 0,
                        error: Some(format!("{:#}", e)),
                    });
                }
            }
        }
        // Remove successful entries from the list (reverse iteration so
        // indices stay valid).
        for i in to_remove.iter().rev() {
            self.workspaces.remove(*i);
        }
        if self.workspaces.is_empty() {
            self.list_state.select(None);
        } else if let Some(sel) = self.list_state.selected() {
            if sel >= self.workspaces.len() {
                self.list_state.select(Some(self.workspaces.len() - 1));
            }
        }

        let freed_total: u64 = outcomes
            .iter()
            .filter(|o| o.error.is_none())
            .map(|o| o.freed)
            .sum();
        let errs = outcomes.iter().filter(|o| o.error.is_some()).count();
        if errs == 0 {
            self.status = Some(format!(
                "Freed {} across {} workspaces",
                ByteSize::b(freed_total),
                outcomes.len()
            ));
            self.status_is_error = false;
        } else {
            self.status = Some(format!(
                "Freed {} across {} workspaces ({} error{})",
                ByteSize::b(freed_total),
                outcomes.len() - errs,
                errs,
                if errs == 1 { "" } else { "s" }
            ));
            self.status_is_error = true;
        }
        self.mode = Mode::Results(outcomes);
    }

    fn tick(&mut self) {
        if matches!(self.mode, Mode::Scanning)
            && self.last_tick.elapsed() >= Duration::from_millis(120)
        {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER.len();
            self.last_tick = Instant::now();
        }
    }
}

#[derive(Copy, Clone)]
enum Position {
    First,
    Last,
}

const SPINNER: &[&str] = &["|", "/", "-", "\\"];

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(1),    // body
            Constraint::Length(2), // status + hints
        ])
        .split(area);

    render_header(f, chunks[0], app);
    render_body(f, chunks[1], app);
    render_status(f, chunks[2], app);

    // Overlays.
    match &app.mode {
        Mode::Confirm => render_confirm_dialog(f, area, app),
        Mode::Results(outcomes) => {
            let outcomes = outcomes.clone();
            render_results_dialog(f, area, &outcomes);
        }
        _ => {}
    }
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let title = format!(
        " cargo-wipe  -  root: {}  -  {} workspaces  -  {} total ",
        app.start_dir.display(),
        app.workspaces.len(),
        ByteSize::b(app.total_size()),
    );
    let header = Paragraph::new(Line::from(vec![Span::styled(
        title,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]));
    f.render_widget(header, area);
}

fn render_body(f: &mut Frame, area: Rect, app: &mut App) {
    if matches!(app.mode, Mode::Scanning) && app.workspaces.is_empty() {
        let frame = SPINNER[app.spinner_frame];
        let p = Paragraph::new(vec![
            Line::from(""),
            Line::from(format!(
                "  {} Scanning {}...",
                frame,
                app.start_dir.display()
            )),
            Line::from("  (this may take a moment on large trees)"),
        ])
        .block(Block::default().borders(Borders::ALL).title(" Please wait "));
        f.render_widget(p, area);
        return;
    }

    if app.workspaces.is_empty() {
        let msg = match &app.mode {
            Mode::Scanning => "Scanning...".to_string(),
            _ => format!(
                "No reclaimable target/ directories found under {}.",
                app.start_dir.display()
            ),
        };
        let p = Paragraph::new(msg)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" Workspaces "));
        f.render_widget(p, area);
        return;
    }

    // Column layout: "> " highlight symbol + "[x] " + path + right-aligned size.
    let inner_width = area.width.saturating_sub(2) as usize; // borders
    // Reserve width: highlight symbol (2) + checkbox (4) + spaces + size column.
    let size_col = 12usize;
    let cb_col = 4usize;
    let highlight = 2usize;
    let gutter = 2usize;
    let path_width = inner_width
        .saturating_sub(highlight + cb_col + size_col + gutter)
        .max(10);

    let items: Vec<ListItem> = app
        .workspaces
        .iter()
        .map(|ws| {
            let checkbox = if ws.selected { "[x]" } else { "[ ]" };
            let size = ByteSize::b(ws.target_size).to_string();
            let path_str = ws.root.display().to_string();
            let path_trimmed = if path_str.len() > path_width && path_width > 3 {
                let cut = path_str.len() - (path_width - 3);
                format!("...{}", &path_str[cut..])
            } else {
                path_str
            };
            let line = Line::from(vec![
                Span::styled(
                    format!("{checkbox} "),
                    if ws.selected {
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::raw(format!("{:<width$} ", path_trimmed, width = path_width)),
                Span::styled(
                    format!("{:>width$}", size, width = size_col),
                    Style::default().fg(Color::Yellow),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Workspaces "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    // Line 1: status message + selection summary.
    let style = if app.status_is_error {
        Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White).bg(Color::DarkGray)
    };
    let msg: String = match &app.status {
        Some(s) => s.clone(),
        None => String::new(),
    };
    let selected_info = format!(
        "selected: {} ({})",
        app.selected_count(),
        ByteSize::b(app.selected_size())
    );
    let full = if msg.is_empty() {
        format!(" {} ", selected_info)
    } else {
        format!(" {}  |  {} ", msg, selected_info)
    };
    let status = Paragraph::new(Line::from(Span::styled(full, style)));
    f.render_widget(status, chunks[0]);

    // Line 2: key hints.
    let hints = " [j/k or up/down] move   [space] toggle   [a] select all   [d] delete   [r] rescan   [g/G] top/bottom   [q] quit ";
    let hint_widget = Paragraph::new(Line::from(Span::styled(
        hints,
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(hint_widget, chunks[1]);
}

fn render_confirm_dialog(f: &mut Frame, area: Rect, app: &App) {
    let count = app.selected_count();
    let size = app.selected_size();
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "  Delete {} workspace{}, freeing {}?",
                count,
                if count == 1 { "" } else { "s" },
                ByteSize::b(size)
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  This will permanently remove the target/ directory in each."),
        Line::from(""),
        Line::from(Span::styled(
            "  [y] yes    [n]/Esc cancel",
            Style::default().fg(Color::Yellow),
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Confirm ")
        .style(Style::default().bg(Color::Black).fg(Color::White));
    let p = Paragraph::new(text).block(block).wrap(Wrap { trim: false });

    let rect = centered_rect(60, 30, area);
    f.render_widget(Clear, rect);
    f.render_widget(p, rect);
}

fn render_results_dialog(f: &mut Frame, area: Rect, outcomes: &[DeleteOutcome]) {
    let ok = outcomes.iter().filter(|o| o.error.is_none()).count();
    let err = outcomes.len() - ok;
    let freed: u64 = outcomes
        .iter()
        .filter(|o| o.error.is_none())
        .map(|o| o.freed)
        .sum();

    let mut lines: Vec<Line> = Vec::with_capacity(outcomes.len() + 4);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "  Deletion complete: {} ok, {} error{}, freed {}",
            ok,
            err,
            if err == 1 { "" } else { "s" },
            ByteSize::b(freed)
        ),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    for o in outcomes {
        let (mark, style) = match &o.error {
            None => ("OK ", Style::default().fg(Color::Green)),
            Some(_) => ("ERR", Style::default().fg(Color::Red)),
        };
        let detail = match &o.error {
            None => format!("freed {}", ByteSize::b(o.freed)),
            Some(e) => e.clone(),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  [{}] ", mark), style),
            Span::raw(format!("{} - {}", o.root.display(), detail)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  [Enter/Esc] dismiss   [r] rescan",
        Style::default().fg(Color::Yellow),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Results ")
        .style(Style::default().bg(Color::Black).fg(Color::White));
    let p = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });

    let rect = centered_rect(80, 70, area);
    f.render_widget(Clear, rect);
    f.render_widget(p, rect);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

// ---------------------------------------------------------------------------
// Event handling
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

fn handle_key(app: &mut App, key: event::KeyEvent) -> Flow {
    // Ctrl-C quits from any mode.
    if let KeyCode::Char('c') = key.code {
        if key.modifiers.contains(event::KeyModifiers::CONTROL) {
            return Flow::Quit;
        }
    }

    match &app.mode {
        Mode::Scanning => {
            if matches!(
                key.code,
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc
            ) {
                return Flow::Quit;
            }
        }
        Mode::Browsing => match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => return Flow::Quit,
            KeyCode::Char('j') | KeyCode::Down => app.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => app.move_cursor(-1),
            KeyCode::PageDown => app.move_cursor(10),
            KeyCode::PageUp => app.move_cursor(-10),
            KeyCode::Char('g') | KeyCode::Home => app.jump_to(Position::First),
            KeyCode::Char('G') | KeyCode::End => app.jump_to(Position::Last),
            KeyCode::Char(' ') => app.toggle_current(),
            KeyCode::Char('a') | KeyCode::Char('A') => app.select_all(),
            KeyCode::Char('r') | KeyCode::Char('R') => app.begin_rescan(),
            KeyCode::Char('d') | KeyCode::Char('D') => {
                if app.selected_count() == 0 {
                    app.status =
                        Some("Nothing selected - use [space] to select workspaces".into());
                    app.status_is_error = true;
                } else {
                    app.mode = Mode::Confirm;
                }
            }
            _ => {}
        },
        Mode::Confirm => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.run_delete();
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.mode = Mode::Browsing;
                app.status = Some("Deletion cancelled".into());
                app.status_is_error = false;
            }
            _ => {}
        },
        Mode::Results(_) => match key.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char(' ') => {
                app.mode = Mode::Browsing;
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                app.begin_rescan();
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => return Flow::Quit,
            _ => {}
        },
    }
    Flow::Continue
}

// ---------------------------------------------------------------------------
// Terminal lifecycle
// ---------------------------------------------------------------------------

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Tui> {
    enable_raw_mode().context("enabling raw mode")?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture).context("entering alternate screen")?;
    let backend = CrosstermBackend::new(out);
    let terminal = Terminal::new(backend).context("creating terminal")?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .ok();
    terminal.show_cursor().ok();
    Ok(())
}

/// Raw restore used by the panic hook, where we don't have a `Terminal` handle.
fn restore_terminal_raw() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
}

fn install_panic_hook() {
    let default = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_terminal_raw();
        default(info);
    }));
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

fn run(app: &mut App, terminal: &mut Tui) -> Result<()> {
    let tick_rate = Duration::from_millis(100);
    let mut last_draw = Instant::now() - tick_rate;

    loop {
        let scan_changed = app.poll_scan();
        app.tick();

        if scan_changed || last_draw.elapsed() >= tick_rate {
            terminal.draw(|f| render(f, app))?;
            last_draw = Instant::now();
        }

        if event::poll(Duration::from_millis(50)).context("polling events")? {
            match event::read().context("reading event")? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if handle_key(app, k) == Flow::Quit {
                        return Ok(());
                    }
                }
                Event::Resize(_, _) => {
                    // Force a redraw; ratatui handles the actual resize.
                    terminal.draw(|f| render(f, app))?;
                    last_draw = Instant::now();
                }
                _ => {}
            }
        }
    }
}

fn determine_start_dir() -> PathBuf {
    let mut args = std::env::args().skip(1);
    // Support both `cargo-wipe <path>` and `cargo wipe <path>` (cargo passes "wipe"
    // as argv[1] when invoked as a subcommand).
    if let Some(first) = args.next() {
        if first == "wipe" {
            if let Some(path) = args.next() {
                return PathBuf::from(path);
            }
        } else {
            return PathBuf::from(first);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(".")
}

fn main() -> Result<()> {
    install_panic_hook();

    let requested = determine_start_dir();
    let start = requested
        .canonicalize()
        .unwrap_or_else(|_| requested.clone());

    let mut terminal = setup_terminal()?;
    let mut app = App::new(start);

    let result = run(&mut app, &mut terminal);

    // Always restore the terminal, even if `run` returned an error.
    restore_terminal(&mut terminal)?;

    result
}
