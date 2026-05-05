use std::cmp::min;
use std::env;
use std::io::{self, Stdout};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Terminal;

use crate::api;
use crate::model::{AccountRecord, QuotaState};
use crate::oauth;
use crate::storage;

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

fn theme_pink() -> Color {
    if use_ansi_palette() {
        return Color::LightMagenta;
    }
    Color::Rgb(255, 102, 170)
}

fn theme_cyan() -> Color {
    if use_ansi_palette() {
        return Color::LightCyan;
    }
    Color::Rgb(68, 196, 255)
}

fn theme_green() -> Color {
    if use_ansi_palette() {
        return Color::LightGreen;
    }
    Color::Rgb(92, 212, 120)
}

fn theme_violet() -> Color {
    if use_ansi_palette() {
        return Color::Magenta;
    }
    Color::Rgb(132, 107, 255)
}

fn theme_text() -> Color {
    if use_ansi_palette() {
        return Color::White;
    }
    Color::Rgb(230, 225, 218)
}

fn theme_muted() -> Color {
    if use_ansi_palette() {
        return Color::DarkGray;
    }
    Color::Rgb(117, 124, 134)
}

fn theme_border() -> Color {
    if use_ansi_palette() {
        return Color::DarkGray;
    }
    Color::Rgb(56, 77, 103)
}

fn theme_selection_bg() -> Color {
    if use_ansi_palette() {
        return Color::Blue;
    }
    Color::Rgb(38, 78, 123)
}

fn theme_warning() -> Color {
    if use_ansi_palette() {
        return Color::Yellow;
    }
    Color::Rgb(255, 196, 82)
}

fn theme_danger() -> Color {
    if use_ansi_palette() {
        return Color::LightRed;
    }
    Color::Rgb(255, 98, 98)
}

fn use_ansi_palette() -> bool {
    if let Ok(force) = env::var("CAS_FORCE_TRUECOLOR") {
        let normalized = force.trim().to_ascii_lowercase();
        if matches!(normalized.as_str(), "1" | "true" | "yes" | "on") {
            return false;
        }
    }

    let colorterm = env::var("COLORTERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        return false;
    }

    let term = env::var("TERM").unwrap_or_default().to_ascii_lowercase();
    if term.contains("direct") || term.contains("truecolor") || term.contains("24bit") {
        return false;
    }

    cfg!(target_os = "macos")
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let popup_width = area.width.saturating_mul(percent_x).saturating_div(100);
    let popup_height = height.min(area.height.saturating_sub(2)).max(3);
    Rect {
        x: area.x + area.width.saturating_sub(popup_width) / 2,
        y: area.y + area.height.saturating_sub(popup_height) / 2,
        width: popup_width.max(24),
        height: popup_height,
    }
}

pub fn run() -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;

    let (tx, rx) = mpsc::channel();
    let mut app = App::new(tx.clone())?;
    let result = event_loop(&mut terminal, &mut app, &rx, tx);

    disable_raw_mode().ok();
    io::stdout().execute(LeaveAlternateScreen).ok();
    result
}

fn event_loop(
    terminal: &mut TuiTerminal,
    app: &mut App,
    rx: &Receiver<WorkerEvent>,
    tx: Sender<WorkerEvent>,
) -> Result<()> {
    loop {
        while let Ok(message) = rx.try_recv() {
            app.handle_worker(message, tx.clone())?;
        }

        terminal.draw(|frame| app.draw(frame))?;
        if app.should_quit {
            return Ok(());
        }

        if event::poll(Duration::from_millis(100)).context("failed to poll terminal events")? {
            if let Event::Key(key) = event::read().context("failed to read terminal event")? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                app.handle_key(key.code, tx.clone())?;
            }
        }
    }
}

#[derive(Clone, Debug)]
enum ListItem {
    Account(usize),
    ExhaustedSummary(usize),
}

#[derive(Debug)]
enum WorkerEvent {
    QuotaLoaded(AccountRecord),
    QuotaFailed { key: String, error: String },
    AuthFinished {
        result: Result<AccountRecord, String>,
        mode: AuthFlowMode,
    },
}

#[derive(Clone, Copy, Debug)]
enum AuthFlowMode {
    Add,
    Relogin,
}

impl AuthFlowMode {
    fn verb(self) -> &'static str {
        match self {
            Self::Add => "login",
            Self::Relogin => "re-login",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ActionMenuItem {
    ApplyCodex,
    ApplyRestartCodex,
    Relogin,
    Refresh,
    Delete,
    Cancel,
}

struct AuthLinkPopup {
    mode: AuthFlowMode,
    url: String,
}

impl ActionMenuItem {
    fn all() -> [Self; 6] {
        [
            Self::ApplyCodex,
            Self::ApplyRestartCodex,
            Self::Relogin,
            Self::Refresh,
            Self::Delete,
            Self::Cancel,
        ]
    }

    fn label(self) -> &'static str {
        match self {
            Self::ApplyCodex => "Apply to Codex",
            Self::ApplyRestartCodex => "Apply and restart Codex",
            Self::Relogin => "Re-login account",
            Self::Refresh => "Refresh Quota",
            Self::Delete => "Delete Account",
            Self::Cancel => "Cancel",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlanFilter {
    All,
    Plus,
    Pro,
    Team,
    Business,
    Free,
    Unknown,
}

impl PlanFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Plus,
            Self::Plus => Self::Pro,
            Self::Pro => Self::Team,
            Self::Team => Self::Business,
            Self::Business => Self::Free,
            Self::Free => Self::Unknown,
            Self::Unknown => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Plus => "plus",
            Self::Pro => "pro",
            Self::Team => "team",
            Self::Business => "business",
            Self::Free => "free",
            Self::Unknown => "unknown",
        }
    }

    fn matches(self, account: &AccountRecord) -> bool {
        if self == Self::All {
            return true;
        }
        account.plan_type().eq_ignore_ascii_case(self.label())
    }
}

struct App {
    accounts: Vec<AccountRecord>,
    selected: usize,
    scroll: usize,
    show_exhausted: bool,
    status: String,
    viewport_rows: usize,
    should_quit: bool,
    add_in_progress: bool,
    search_mode: bool,
    search_input: String,
    plan_filter: PlanFilter,
    action_menu_open: bool,
    action_menu_cursor: usize,
    auth_popup: Option<AuthLinkPopup>,
}

impl App {
    fn new(tx: Sender<WorkerEvent>) -> Result<Self> {
        let mut app = Self {
            accounts: storage::load_accounts()?,
            selected: 0,
            scroll: 0,
            show_exhausted: false,
            status: String::from(
                "Enter actions • r refresh • R refresh all • / search • p cycle plan filter • f clear filters • q quit",
            ),
            viewport_rows: 5,
            should_quit: false,
            add_in_progress: false,
            search_mode: false,
            search_input: String::new(),
            plan_filter: PlanFilter::All,
            action_menu_open: false,
            action_menu_cursor: 0,
            auth_popup: None,
        };
        app.ensure_selection_valid();
        app.fetch_missing(tx);
        Ok(app)
    }

    fn handle_key(&mut self, code: KeyCode, tx: Sender<WorkerEvent>) -> Result<()> {
        if self.auth_popup.is_some() {
            match code {
                KeyCode::Esc | KeyCode::Enter => self.auth_popup = None,
                _ => {}
            }
            return Ok(());
        }

        if self.action_menu_open {
            match code {
                KeyCode::Esc => self.close_action_menu(),
                KeyCode::Up | KeyCode::Char('k') => {
                    self.action_menu_cursor = self.action_menu_cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.action_menu_cursor =
                        (self.action_menu_cursor + 1).min(ActionMenuItem::all().len() - 1);
                }
                KeyCode::Enter => {
                    self.execute_action_menu(tx)?;
                }
                _ => {}
            }
            return Ok(());
        }

        if self.search_mode {
            match code {
                KeyCode::Esc => {
                    self.search_mode = false;
                }
                KeyCode::Enter => {
                    self.search_mode = false;
                    self.selected = 0;
                    self.scroll = 0;
                }
                KeyCode::Backspace => {
                    self.search_input.pop();
                    self.selected = 0;
                    self.scroll = 0;
                }
                KeyCode::Char(ch) => {
                    self.search_input.push(ch);
                    self.selected = 0;
                    self.scroll = 0;
                }
                _ => {}
            }
            self.ensure_selection_valid();
            return Ok(());
        }

        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('e') => self.toggle_exhausted(),
            KeyCode::Char('/') => {
                self.search_mode = true;
                self.status = "search accounts by email/label".to_string();
            }
            KeyCode::Char('p') => {
                self.plan_filter = self.plan_filter.next();
                self.selected = 0;
                self.scroll = 0;
                self.status = format!("plan filter: {}", self.plan_filter.label());
            }
            KeyCode::Char('f') => {
                self.plan_filter = PlanFilter::All;
                self.search_input.clear();
                self.search_mode = false;
                self.selected = 0;
                self.scroll = 0;
                self.status = "cleared filters".to_string();
            }
            KeyCode::Char('r') => self.refresh_selected(tx),
            KeyCode::Char('R') => self.refresh_all(tx),
            KeyCode::Enter => self.open_action_menu_or_toggle_exhausted(),
            KeyCode::Char('s') => self.apply_selected_to_codex()?,
            KeyCode::Char('l') => self.relogin_selected(tx),
            KeyCode::Char('d') => self.delete_selected(tx)?,
            KeyCode::Char('a') => self.add_account(tx),
            _ => {}
        }
        Ok(())
    }

    fn handle_worker(&mut self, message: WorkerEvent, tx: Sender<WorkerEvent>) -> Result<()> {
        match message {
            WorkerEvent::QuotaLoaded(account) => {
                self.replace_account(account);
                self.status = "quota refreshed".to_string();
            }
            WorkerEvent::QuotaFailed { key, error } => {
                if let Some(account) = self
                    .accounts
                    .iter_mut()
                    .find(|account| account.key() == key)
                {
                    account.quota = QuotaState::Error(error.clone());
                }
                self.status = error;
            }
            WorkerEvent::AuthFinished { result, mode } => {
                self.add_in_progress = false;
                self.auth_popup = None;
                match result {
                    Ok(account) => {
                        storage::upsert_managed_account(&account)?;
                        self.reload_accounts(tx);
                        self.select_account_key(&account.key());
                        self.status = match mode {
                            AuthFlowMode::Add => format!("added {}", account.display_name()),
                            AuthFlowMode::Relogin => {
                                format!("re-logged {}", account.display_name())
                            }
                        };
                    }
                    Err(error) => self.status = error,
                }
            }
        }
        self.ensure_selection_valid();
        Ok(())
    }

    fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(2),
            ])
            .split(frame.area());

        let body_areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(7)])
            .split(areas[1]);

        self.viewport_rows = body_areas[0].height.saturating_sub(3) as usize;
        if self.viewport_rows == 0 {
            self.viewport_rows = 1;
        }
        self.ensure_visible();

        let title = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(self.status.as_str(), Style::default().fg(theme_muted())),
                Span::raw("  "),
                Span::styled(
                    if self.add_in_progress {
                        "login in progress..."
                    } else {
                        ""
                    },
                    Style::default().fg(theme_cyan()),
                ),
            ]),
            Line::from(vec![
                Span::styled("Filter", Style::default().fg(theme_muted())),
                Span::raw(": "),
                Span::styled(self.plan_filter.label(), Style::default().fg(theme_pink())),
                Span::styled(" (p cycle)", Style::default().fg(theme_muted())),
                Span::raw("  "),
                Span::styled("Search", Style::default().fg(theme_muted())),
                Span::raw(": "),
                Span::styled(
                    if self.search_input.is_empty() {
                        if self.search_mode {
                            "_"
                        } else {
                            "none"
                        }
                    } else {
                        self.search_input.as_str()
                    },
                    Style::default().fg(if self.search_mode {
                        theme_cyan()
                    } else {
                        theme_green()
                    }),
                ),
                Span::styled(" (/ edit, f clear)", Style::default().fg(theme_muted())),
            ]),
        ]);
        frame.render_widget(title, areas[0]);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme_border()));
        let inner = block.inner(body_areas[0]);
        frame.render_widget(block, body_areas[0]);

        let items = self.list_items();
        let start = self.scroll.min(items.len().saturating_sub(1));
        let end = min(items.len(), start + self.viewport_rows);
        let visible = &items[start..end];

        let rows: Vec<Row> = visible
            .iter()
            .enumerate()
            .map(|(offset, item)| self.render_row(item, start + offset))
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(2),
                Constraint::Length(8),
                Constraint::Percentage(34),
                Constraint::Percentage(24),
                Constraint::Percentage(24),
            ],
        )
        .header(
            Row::new(vec![
                Cell::from(" "),
                Cell::from("Active"),
                Cell::from("Account"),
                Cell::from("5h"),
                Cell::from("Week"),
            ])
            .style(
                Style::default()
                    .fg(theme_cyan())
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .column_spacing(1);
        frame.render_widget(table, inner);

        self.draw_details(frame, body_areas[1]);
        if self.action_menu_open {
            self.draw_action_menu(frame);
        }
        if self.auth_popup.is_some() {
            self.draw_auth_popup(frame);
        }

        let footer = Paragraph::new(vec![Line::from(vec![
            Span::styled(
                "↑↓",
                Style::default()
                    .fg(Color::Rgb(80, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" move  "),
            Span::styled(
                "e",
                Style::default()
                    .fg(Color::Rgb(80, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" exhausted  "),
            Span::styled(
                "/",
                Style::default()
                    .fg(Color::Rgb(80, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" search  "),
            Span::styled(
                "p",
                Style::default()
                    .fg(Color::Rgb(80, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" cycle plan  "),
            Span::styled(
                "f",
                Style::default()
                    .fg(Color::Rgb(80, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" clear filters  "),
            Span::styled(
                "Enter",
                Style::default()
                    .fg(Color::Rgb(80, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" menu  "),
            Span::styled(
                "s",
                Style::default()
                    .fg(Color::Rgb(80, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" codex  "),
            Span::styled(
                "a",
                Style::default()
                    .fg(Color::Rgb(80, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" add  "),
            Span::styled(
                "l",
                Style::default()
                    .fg(Color::Rgb(80, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" re-login  "),
            Span::styled(
                "d",
                Style::default()
                    .fg(Color::Rgb(80, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" delete  "),
            Span::styled(
                "r",
                Style::default()
                    .fg(Color::Rgb(80, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" refresh  "),
            Span::styled(
                "R",
                Style::default()
                    .fg(Color::Rgb(80, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" refresh all  "),
            Span::styled(
                "q",
                Style::default()
                    .fg(Color::Rgb(80, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" quit"),
        ])])
        .wrap(Wrap { trim: true });
        frame.render_widget(footer, areas[2]);
    }

    fn render_row(&self, item: &ListItem, absolute_index: usize) -> Row<'static> {
        let selected = absolute_index == self.selected;
        let selected_style = Style::default()
            .fg(theme_text())
            .bg(theme_selection_bg())
            .add_modifier(Modifier::BOLD);

        match item {
            ListItem::Account(index) => {
                let account = &self.accounts[*index];
                let marker = if selected { "▶" } else { " " };
                let account_cell = account_cell(account);
                let five_hour = quota_cell(&account.quota, 18_000);
                let weekly = quota_cell(&account.quota, 604_800);
                let active = active_badges(account);

                let row = Row::new(vec![
                    Cell::from(marker.to_string()),
                    Cell::from(active),
                    Cell::from(account_cell),
                    Cell::from(five_hour),
                    Cell::from(weekly),
                ]);
                if selected {
                    row.style(selected_style)
                } else {
                    row
                }
            }
            ListItem::ExhaustedSummary(count) => {
                let row = Row::new(vec![
                    Cell::from(if selected { "▶" } else { " " }.to_string()),
                    Cell::from(""),
                    Cell::from(format!("{} exhausted accounts • press e to show", count)),
                    Cell::from(""),
                    Cell::from(""),
                ]);
                if selected {
                    row.style(selected_style)
                } else {
                    row.style(Style::default().fg(theme_warning()))
                }
            }
        }
    }

    fn draw_details(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Details")
            .border_style(Style::default().fg(theme_border()))
            .title_style(
                Style::default()
                    .fg(theme_pink())
                    .add_modifier(Modifier::BOLD),
            );
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(index) = self.selected_account_index() else {
            frame.render_widget(
                Paragraph::new("No account selected").style(Style::default().fg(theme_muted())),
                inner,
            );
            return;
        };

        let account = &self.accounts[index];
        let lines = vec![
            Line::from(vec![
                label_span("Email"),
                Span::raw(": "),
                Span::styled(account.email.as_str(), Style::default().fg(theme_text())),
                Span::raw("  "),
                label_span("Plan"),
                Span::raw(": "),
                plan_badge_span(account.plan_type()),
                Span::raw("  "),
                label_span("Managed"),
                Span::raw(": "),
                Span::styled(
                    if account.managed { "yes" } else { "no" },
                    yes_no_style(account.managed),
                ),
            ]),
            Line::from(vec![
                label_span("Active"),
                Span::raw(": "),
                Span::styled(active_badges(account), Style::default().fg(theme_green())),
                Span::raw("  "),
                label_span("Account ID"),
                Span::raw(": "),
                Span::styled(
                    short_account_id(&account.account_id),
                    Style::default().fg(theme_cyan()),
                ),
            ]),
            Line::from(vec![
                label_span("5h Reset"),
                Span::raw(": "),
                Span::styled(
                    quota_reset_text(&account.quota, 18_000),
                    Style::default().fg(theme_text()),
                ),
                Span::raw("  "),
                label_span("Week Reset"),
                Span::raw(": "),
                Span::styled(
                    quota_reset_text(&account.quota, 604_800),
                    Style::default().fg(theme_text()),
                ),
            ]),
            Line::from(vec![
                label_span("Token Exp"),
                Span::raw(": "),
                Span::styled(
                    format_expiry(account.expires_at),
                    Style::default().fg(theme_text()),
                ),
                Span::raw("  "),
                label_span("Status"),
                Span::raw(": "),
                Span::styled(account_status_text(account), status_style(account)),
            ]),
        ];

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
    }

    fn move_selection(&mut self, delta: isize) {
        let items = self.list_items();
        if items.is_empty() {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        let max_index = items.len().saturating_sub(1) as isize;
        let next = (self.selected as isize + delta).clamp(0, max_index) as usize;
        self.selected = next;
        self.ensure_visible();
    }

    fn open_action_menu_or_toggle_exhausted(&mut self) {
        match self.list_items().get(self.selected) {
            Some(ListItem::ExhaustedSummary(_)) => self.toggle_exhausted(),
            Some(ListItem::Account(_)) => {
                self.action_menu_open = true;
                self.action_menu_cursor = 0;
                self.status = "enter to choose action • esc to close".to_string();
            }
            None => {}
        }
    }

    fn close_action_menu(&mut self) {
        self.action_menu_open = false;
        self.action_menu_cursor = 0;
    }

    fn execute_action_menu(&mut self, tx: Sender<WorkerEvent>) -> Result<()> {
        let action = ActionMenuItem::all()[self.action_menu_cursor];
        self.close_action_menu();
        match action {
            ActionMenuItem::ApplyCodex => self.apply_selected_to_codex()?,
            ActionMenuItem::ApplyRestartCodex => self.apply_selected_to_codex_and_restart()?,
            ActionMenuItem::Relogin => self.relogin_selected(tx),
            ActionMenuItem::Refresh => self.refresh_selected(tx),
            ActionMenuItem::Delete => self.delete_selected(tx)?,
            ActionMenuItem::Cancel => {}
        }
        Ok(())
    }

    fn draw_action_menu(&self, frame: &mut ratatui::Frame<'_>) {
        let area = centered_rect(34, 14, frame.area());
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Actions")
            .border_style(Style::default().fg(theme_border()))
            .title_style(
                Style::default()
                    .fg(theme_pink())
                    .add_modifier(Modifier::BOLD),
            );
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines = ActionMenuItem::all()
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                let selected = idx == self.action_menu_cursor;
                Line::from(vec![Span::styled(
                    format!("{} {}", if selected { "▶" } else { " " }, item.label()),
                    if selected {
                        Style::default()
                            .fg(theme_text())
                            .bg(theme_selection_bg())
                            .add_modifier(Modifier::BOLD)
                    } else if matches!(item, ActionMenuItem::Delete) {
                        Style::default().fg(theme_warning())
                    } else {
                        Style::default().fg(theme_text())
                    },
                )])
            })
            .collect::<Vec<_>>();

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
    }

    fn draw_auth_popup(&self, frame: &mut ratatui::Frame<'_>) {
        let Some(popup) = &self.auth_popup else {
            return;
        };

        let area = centered_rect(72, 10, frame.area());
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Login Link")
            .border_style(Style::default().fg(theme_border()))
            .title_style(
                Style::default()
                    .fg(theme_pink())
                    .add_modifier(Modifier::BOLD),
            );
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines = vec![
            Line::from(vec![
                Span::styled(
                    format!("Open this {} URL in any browser.", popup.mode.verb()),
                    Style::default().fg(theme_text()),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    "The app is waiting for the localhost callback on port 1455.",
                    Style::default().fg(theme_muted()),
                ),
            ]),
            Line::default(),
            Line::from(vec![Span::styled(
                popup.url.as_str(),
                Style::default()
                    .fg(theme_cyan())
                    .add_modifier(Modifier::UNDERLINED),
            )]),
            Line::default(),
            Line::from(vec![Span::styled(
                "Esc or Enter hides this popup while login keeps running.",
                Style::default().fg(theme_muted()),
            )]),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn toggle_exhausted(&mut self) {
        let items = self.list_items();
        let exhausted_count = self.exhausted_indices().len();
        if exhausted_count == 0 {
            return;
        }
        let selected_was_summary = matches!(
            items.get(self.selected),
            Some(ListItem::ExhaustedSummary(_))
        );
        self.show_exhausted = !self.show_exhausted;
        self.ensure_selection_valid();

        if self.show_exhausted && selected_was_summary {
            let visible_count = self.non_exhausted_indices().len();
            self.selected = visible_count.min(self.list_items().len().saturating_sub(1));
        }
        if !self.show_exhausted
            && matches!(
                self.list_items().get(self.selected),
                Some(ListItem::ExhaustedSummary(_))
            )
        {
            self.selected = self.non_exhausted_indices().len();
        }
        self.ensure_visible();
    }

    fn refresh_selected(&mut self, tx: Sender<WorkerEvent>) {
        if let Some(index) = self.selected_account_index() {
            self.start_fetch(index, tx);
        }
    }

    fn refresh_all(&mut self, tx: Sender<WorkerEvent>) {
        for index in 0..self.accounts.len() {
            self.start_fetch(index, tx.clone());
        }
    }

    fn add_account(&mut self, tx: Sender<WorkerEvent>) {
        if self.add_in_progress {
            return;
        }
        if let Err(error) = self.start_auth_flow(AuthFlowMode::Add, tx) {
            self.status = error.to_string();
        }
    }

    fn relogin_selected(&mut self, tx: Sender<WorkerEvent>) {
        if self.add_in_progress || self.selected_account_index().is_none() {
            return;
        }
        if let Err(error) = self.start_auth_flow(AuthFlowMode::Relogin, tx) {
            self.status = error.to_string();
        }
    }

    fn start_auth_flow(&mut self, mode: AuthFlowMode, tx: Sender<WorkerEvent>) -> Result<()> {
        if self.add_in_progress {
            return Ok(());
        }

        let session = oauth::begin_login_session()?;
        self.add_in_progress = true;
        self.status = format!("open the {} link and complete login...", mode.verb());
        self.auth_popup = Some(AuthLinkPopup {
            mode: mode.clone(),
            url: session.auth_url().to_string(),
        });

        thread::spawn(move || {
            let result = session.run().map_err(|error| error.to_string());
            let _ = tx.send(WorkerEvent::AuthFinished { result, mode });
        });
        Ok(())
    }

    fn delete_selected(&mut self, tx: Sender<WorkerEvent>) -> Result<()> {
        if let Some(index) = self.selected_account_index() {
            let account = self.accounts[index].clone();
            storage::delete_account(&account)?;
            self.status = format!("deleted {}", account.display_name());
            self.reload_accounts(tx);
        }
        Ok(())
    }

    fn apply_selected_to_codex(&mut self) -> Result<()> {
        self.apply_selected_to_codex_internal(false)
    }

    fn apply_selected_to_codex_and_restart(&mut self) -> Result<()> {
        self.apply_selected_to_codex_internal(true)
    }

    fn apply_selected_to_codex_internal(&mut self, restart: bool) -> Result<()> {
        if let Some(index) = self.selected_account_index() {
            let account = self.accounts[index].clone();
            let path = storage::apply_account_to_codex(&account)?;
            for account in &mut self.accounts {
                account.codex_active = false;
            }
            if let Some(account) = self.accounts.get_mut(index) {
                account.codex_active = true;
            }
            if restart {
                restart_codex_app()?;
                self.status = format!("applied to Codex and restarted app: {}", path.display());
            } else {
                self.status = format!("applied to Codex: {}", path.display());
            }
        }
        Ok(())
    }

    fn reload_accounts(&mut self, tx: Sender<WorkerEvent>) {
        let selected_key = self
            .selected_account_index()
            .map(|index| self.accounts[index].key());
        if let Ok(accounts) = storage::load_accounts() {
            self.accounts = accounts;
            if let Some(key) = selected_key {
                self.select_account_key(&key);
            }
            self.fetch_missing(tx);
            self.ensure_selection_valid();
        }
    }

    fn fetch_missing(&mut self, tx: Sender<WorkerEvent>) {
        for index in 0..self.accounts.len() {
            if matches!(self.accounts[index].quota, QuotaState::Idle) {
                self.start_fetch(index, tx.clone());
            }
        }
    }

    fn start_fetch(&mut self, index: usize, tx: Sender<WorkerEvent>) {
        if let Some(account) = self.accounts.get_mut(index) {
            if matches!(account.quota, QuotaState::Loading) {
                return;
            }
            account.quota = QuotaState::Loading;
            let snapshot = account.clone();
            thread::spawn(move || match api::fetch_quota(snapshot.clone()) {
                Ok(updated) => {
                    let _ = tx.send(WorkerEvent::QuotaLoaded(updated));
                }
                Err(error) => {
                    let _ = tx.send(WorkerEvent::QuotaFailed {
                        key: snapshot.key(),
                        error: error.to_string(),
                    });
                }
            });
        }
    }

    fn replace_account(&mut self, updated: AccountRecord) {
        if let Some(index) = self
            .accounts
            .iter()
            .position(|account| account.key() == updated.key())
        {
            self.accounts[index].label = updated.label;
            self.accounts[index].email = updated.email;
            self.accounts[index].account_id = updated.account_id;
            self.accounts[index].access_token = updated.access_token;
            self.accounts[index].refresh_token = updated.refresh_token;
            self.accounts[index].expires_at = updated.expires_at;
            self.accounts[index].client_id = updated.client_id;
            self.accounts[index].quota = updated.quota;
        }
    }

    fn select_account_key(&mut self, key: &str) {
        let items = self.list_items();
        if let Some(pos) = items.iter().position(|item| match item {
            ListItem::Account(index) => self.accounts[*index].key() == key,
            ListItem::ExhaustedSummary(_) => false,
        }) {
            self.selected = pos;
            self.ensure_visible();
        }
    }

    fn selected_account_index(&self) -> Option<usize> {
        match self.list_items().get(self.selected) {
            Some(ListItem::Account(index)) => Some(*index),
            _ => None,
        }
    }

    fn ordered_indices(&self) -> Vec<usize> {
        let mut indices = (0..self.accounts.len()).collect::<Vec<_>>();
        indices.sort_by_key(|index| self.accounts[*index].sort_tuple());
        indices
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let query = self.search_input.trim().to_lowercase();
        self.ordered_indices()
            .into_iter()
            .filter(|index| {
                let account = &self.accounts[*index];
                if !self.plan_filter.matches(account) {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                let haystacks = [
                    account.display_name().to_lowercase(),
                    account.email.to_lowercase(),
                    account.account_id.to_lowercase(),
                ];
                haystacks.iter().any(|value| value.contains(&query))
            })
            .collect()
    }

    fn non_exhausted_indices(&self) -> Vec<usize> {
        self.filtered_indices()
            .into_iter()
            .filter(|index| !self.accounts[*index].is_exhausted())
            .collect()
    }

    fn exhausted_indices(&self) -> Vec<usize> {
        self.filtered_indices()
            .into_iter()
            .filter(|index| self.accounts[*index].is_exhausted())
            .collect()
    }

    fn list_items(&self) -> Vec<ListItem> {
        let mut items = self
            .non_exhausted_indices()
            .into_iter()
            .map(ListItem::Account)
            .collect::<Vec<_>>();
        let exhausted = self.exhausted_indices();
        if exhausted.is_empty() {
            return items;
        }
        if self.show_exhausted {
            items.extend(exhausted.into_iter().map(ListItem::Account));
        } else {
            items.push(ListItem::ExhaustedSummary(exhausted.len()));
        }
        items
    }

    fn ensure_selection_valid(&mut self) {
        let items = self.list_items();
        if items.is_empty() {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        if self.selected >= items.len() {
            self.selected = items.len() - 1;
        }
        self.ensure_visible();
    }

    fn ensure_visible(&mut self) {
        let items = self.list_items();
        if items.is_empty() {
            self.scroll = 0;
            return;
        }
        let capacity = self.viewport_rows.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + capacity {
            self.scroll = self.selected + 1 - capacity;
        }
        let max_scroll = items.len().saturating_sub(capacity);
        self.scroll = self.scroll.min(max_scroll);
    }
}

#[cfg(target_os = "windows")]
fn restart_codex_app() -> Result<()> {
    let _ = Command::new("taskkill")
        .args(["/IM", "Codex.exe", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    thread::sleep(Duration::from_millis(900));
    Command::new("explorer.exe")
        .arg("shell:AppsFolder\\OpenAI.Codex_2p2nqsd0c76g0!App")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to relaunch Codex")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn restart_codex_app() -> Result<()> {
    let _ = Command::new("osascript")
        .args(["-e", "tell application \"Codex\" to quit"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    thread::sleep(Duration::from_millis(900));
    Command::new("open")
        .args(["-a", "Codex"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to relaunch Codex")?;
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn restart_codex_app() -> Result<()> {
    anyhow::bail!("Codex restart is only supported on Windows and macOS right now")
}

fn active_badges(account: &AccountRecord) -> String {
    if account.codex_active {
        return format!("{:^8}", "●");
    }
    " ".repeat(8)
}

fn account_cell(account: &AccountRecord) -> Text<'static> {
    Text::from(Line::from(vec![
        Span::styled(
            account.display_name(),
            Style::default()
                .fg(theme_text())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        plan_badge_span(account.plan_type()),
    ]))
}

fn quota_cell(state: &QuotaState, window_sec: i64) -> Text<'static> {
    match state {
        QuotaState::Idle => Text::from(Line::from(Span::styled(
            "queued",
            Style::default().fg(theme_muted()),
        ))),
        QuotaState::Loading => Text::from(Line::from(Span::styled(
            "loading",
            Style::default().fg(theme_cyan()),
        ))),
        QuotaState::Error(error) => Text::from(Line::from(Span::styled(
            truncate(error, 20),
            Style::default().fg(theme_danger()),
        ))),
        QuotaState::Ready(data) => match data.window_by_seconds(window_sec) {
            Some(window) => {
                let bar_style = quota_style(window.left_percent);
                Text::from(Line::from(vec![
                    Span::styled(mini_bar(window.left_percent), bar_style),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:>3.0}%", window.left_percent),
                        bar_style.add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format_reset(window.reset_at),
                        Style::default().fg(theme_muted()),
                    ),
                ]))
            }
            None => Text::from(Line::from(Span::styled(
                "n/a",
                Style::default().fg(theme_muted()),
            ))),
        },
    }
}

fn mini_bar(percent_left: f64) -> String {
    let width = 8;
    let filled = ((percent_left.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

fn format_reset(reset_at: Option<chrono::DateTime<chrono::Utc>>) -> String {
    match reset_at {
        Some(reset_at) => {
            let now = chrono::Utc::now();
            if reset_at <= now {
                return "now".to_string();
            }
            let delta = reset_at - now;
            let days = delta.num_days();
            if days >= 1 {
                let hours = (delta - chrono::Duration::days(days)).num_hours();
                return format!("{}d {}h", days, hours);
            }
            let hours = delta.num_hours();
            let minutes = (delta - chrono::Duration::hours(hours)).num_minutes();
            if hours > 0 {
                format!("{}h {}m", hours, minutes)
            } else {
                format!("{}m", minutes)
            }
        }
        None => "-".to_string(),
    }
}

fn quota_style(percent_left: f64) -> Style {
    if percent_left <= 10.0 {
        Style::default().fg(theme_danger())
    } else if percent_left <= 35.0 {
        Style::default().fg(theme_warning())
    } else if percent_left <= 65.0 {
        Style::default().fg(theme_violet())
    } else {
        Style::default().fg(theme_green())
    }
}

fn truncate(value: &str, max_len: usize) -> String {
    let trimmed = value.trim();
    if trimmed.len() <= max_len {
        return trimmed.to_string();
    }
    format!("{}...", &trimmed[..max_len])
}

fn label_span(label: &'static str) -> Span<'static> {
    Span::styled(
        label,
        Style::default()
            .fg(theme_muted())
            .add_modifier(Modifier::BOLD),
    )
}

fn yes_no_style(value: bool) -> Style {
    Style::default().fg(if value { theme_green() } else { theme_muted() })
}

fn short_account_id(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() <= 12 {
        return trimmed.to_string();
    }
    format!("{}...{}", &trimmed[..6], &trimmed[trimmed.len() - 4..])
}

fn quota_reset_text(state: &QuotaState, window_sec: i64) -> String {
    match state {
        QuotaState::Ready(data) => data
            .window_by_seconds(window_sec)
            .map(|window| format_reset(window.reset_at))
            .unwrap_or_else(|| "-".to_string()),
        QuotaState::Loading => "loading".to_string(),
        QuotaState::Error(_) => "error".to_string(),
        QuotaState::Idle => "queued".to_string(),
    }
}

fn format_expiry(expiry: Option<chrono::DateTime<chrono::Utc>>) -> String {
    match expiry {
        Some(expiry) => expiry
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
        None => "-".to_string(),
    }
}

fn account_status_text(account: &AccountRecord) -> &'static str {
    match &account.quota {
        QuotaState::Loading => "loading",
        QuotaState::Error(_) => "error",
        QuotaState::Idle => "queued",
        QuotaState::Ready(_) if account.is_exhausted() => "exhausted",
        QuotaState::Ready(_) => "ready",
    }
}

fn status_style(account: &AccountRecord) -> Style {
    match &account.quota {
        QuotaState::Loading => Style::default().fg(theme_cyan()),
        QuotaState::Error(_) => Style::default().fg(theme_danger()),
        QuotaState::Idle => Style::default().fg(theme_muted()),
        QuotaState::Ready(_) if account.is_exhausted() => Style::default().fg(theme_warning()),
        QuotaState::Ready(_) => Style::default().fg(theme_green()),
    }
}

fn plan_badge_span(plan: &str) -> Span<'static> {
    let normalized = plan.trim().to_lowercase();
    let (label, fg, bg) = match normalized.as_str() {
        "plus" => ("PLUS", Color::Black, theme_warning()),
        "pro" => ("PRO", Color::Black, theme_pink()),
        "team" => ("TEAM", Color::Black, theme_cyan()),
        "business" => ("BIZ", Color::Black, theme_violet()),
        "free" => ("FREE", Color::Black, theme_muted()),
        _ => ("UNK", Color::Black, theme_border()),
    };
    Span::styled(
        format!(" {} ", label),
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )
}


