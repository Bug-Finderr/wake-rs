use crate::commands;
use crate::error::Result;
use crate::session::{self, Session};
use chrono::{Local, Utc};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::style::{Attribute, Color, ContentStyle, ResetColor, StyledContent};
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use std::fmt::{Display, Write as _};
use std::io::{BufRead, Write as _};

fn style_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM")
            .is_ok_and(|term| !term.trim().is_empty() && !term.eq_ignore_ascii_case("dumb"))
}

#[derive(Clone, Copy)]
struct Theme(bool);

impl Theme {
    fn current() -> Self {
        Self(style_enabled())
    }

    fn paint<D: Display>(
        self,
        content: D,
        color: Option<u8>,
        attribute: Option<Attribute>,
    ) -> StyledContent<D> {
        let mut style = ContentStyle::new();
        if self.0 {
            style.foreground_color = color.map(Color::AnsiValue);
            if let Some(attribute) = attribute {
                style.attributes.set(attribute);
            }
        }
        style.apply(content)
    }

    fn color<D: Display>(self, content: D, color: u8) -> StyledContent<D> {
        self.paint(content, Some(color), None)
    }

    fn bold<D: Display>(self, content: D, color: u8) -> StyledContent<D> {
        self.paint(content, Some(color), Some(Attribute::Bold))
    }

    fn dim<D: Display>(self, content: D) -> StyledContent<D> {
        self.paint(content, None, Some(Attribute::Dim))
    }
}

#[derive(Clone, Copy)]
enum Action {
    ToggleDetail,
    Quit,
    Stop,
    StartIndefinite,
    Ask(&'static str, Option<&'static str>),
}

struct Item {
    label: &'static str,
    hint: &'static str,
    action: Option<Action>,
}

impl Item {
    const fn new(label: &'static str, hint: &'static str, action: Action) -> Self {
        Self {
            label,
            hint,
            action: Some(action),
        }
    }

    const fn separator() -> Self {
        Self {
            label: "",
            hint: "",
            action: None,
        }
    }
}

pub fn run() -> Result<()> {
    let Some((action, no_display)) = Picker::default().pick()? else {
        return Ok(());
    };
    run_action(action, no_display)
}

#[derive(Default)]
struct Terminal {
    active: bool,
    raw: bool,
}

impl Terminal {
    fn enter() -> Self {
        let terminal = Self {
            active: true,
            raw: enable_raw_mode().is_ok(),
        };
        execute!(std::io::stdout(), EnterAlternateScreen, Hide).ok();
        terminal
    }

    fn leave(&mut self) {
        self.leave_to(&mut std::io::stdout());
    }

    fn leave_to(&mut self, output: &mut impl std::io::Write) {
        if !std::mem::take(&mut self.active) {
            return;
        }
        execute!(output, Show, LeaveAlternateScreen, ResetColor).ok();
        if std::mem::take(&mut self.raw) {
            disable_raw_mode().ok();
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.leave();
    }
}

#[derive(Default)]
struct Picker {
    no_display: bool,
    show_detail: bool,
    selected: usize,
}

impl Picker {
    fn pick(&mut self) -> Result<Option<(Action, bool)>> {
        let lock = session::acquire_lock()?;
        commands::recover_stale_lid_session_unlocked()?;
        let existing = session::read_current()?;
        drop(lock);
        let _terminal = Terminal::enter();
        let items = build_menu(existing.is_some());
        self.selected = items
            .iter()
            .position(|item| item.action.is_some())
            .unwrap_or(0);

        loop {
            self.render(&items, &existing);
            let key = read_key()?;
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected = prev(&items, self.selected);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selected = next(&items, self.selected);
                }
                KeyCode::Char('d' | 'D') if existing.is_none() => {
                    self.no_display = !self.no_display;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                KeyCode::Char('q' | 'Q') | KeyCode::Esc => return Ok(None),
                KeyCode::Enter => match items[self.selected].action {
                    Some(Action::ToggleDetail) => self.show_detail = !self.show_detail,
                    Some(Action::Quit) => return Ok(None),
                    Some(action) => return Ok(Some((action, self.no_display))),
                    None => {}
                },
                _ => {}
            }
        }
    }

    fn render(&self, items: &[Item], existing: &Option<Session>) {
        let mut sb = String::with_capacity(2048);
        let theme = Theme::current();
        writeln!(sb, "{}{}", Clear(ClearType::All), MoveTo(0, 0)).ok();
        writeln!(
            sb,
            "  {}  {}",
            theme.bold("☕ wake", 87),
            theme.dim("- keep your machine awake")
        )
        .ok();
        sb.push('\n');

        if let Some(session) = existing {
            let now = Utc::now();
            write!(
                sb,
                "  {}   {} {}",
                theme.color("● active", 220),
                session.spec.trigger.label(),
                session.spec.trigger.detail()
            )
            .ok();
            if let Some(end) = session.ends_at {
                let remaining = commands::pretty_duration((end - now).num_seconds().max(0));
                write!(sb, "   {}", theme.dim(format!("({remaining} left)"))).ok();
            }
            sb.push('\n');
            if self.show_detail {
                append_detail(&mut sb, session, now);
            }
            sb.push('\n');
        } else {
            writeln!(sb, "  {}\n", theme.color("○ no active session", 245)).ok();
        }

        for (index, item) in items.iter().enumerate() {
            if item.action.is_none() {
                writeln!(sb, "    {}", theme.dim("─────────────────────────")).ok();
            } else if index == self.selected {
                write!(sb, "  {}", theme.bold(format!("▸ {}", item.label), 213)).ok();
                if !item.hint.is_empty() {
                    write!(sb, "   {}", theme.dim(item.hint)).ok();
                }
                sb.push('\n');
            } else {
                writeln!(sb, "    {}", item.label).ok();
            }
        }

        let mut footer = "↑↓/jk navigate · ↵ select".to_string();
        if existing.is_none() {
            write!(
                footer,
                " · d display-sleep [{}]",
                if self.no_display { "ON" } else { "off" }
            )
            .ok();
        }
        footer.push_str(" · q quit");
        writeln!(sb, "\n  {}", theme.dim(footer)).ok();

        print!("{sb}");
        std::io::stdout().flush().ok();
    }
}

fn read_key() -> Result<crossterm::event::KeyEvent> {
    loop {
        if let Event::Key(key) =
            event::read().map_err(|error| crate::error::AppError::fail(error.to_string()))?
            && key.kind == KeyEventKind::Press
        {
            return Ok(key);
        }
    }
}

fn run_action(action: Action, no_display: bool) -> Result<()> {
    match action {
        Action::Stop => commands::stop(),
        Action::StartIndefinite => start_indefinite(no_display),
        Action::Ask(prompt, flag) => ask_and_start(no_display, prompt, flag),
        Action::ToggleDetail | Action::Quit => Ok(()),
    }
}

fn start_indefinite(no_display: bool) -> Result<()> {
    let even_lid = crate::platform::supports_even_lid()
        && ask_yes_no("Keep awake with the lid closed too? (needs sudo)");
    commands::start(&start_args(
        no_display,
        if even_lid { &["--even-lid"] } else { &[] },
    ))
}

fn ask_and_start(no_display: bool, prompt: &str, flag: Option<&str>) -> Result<()> {
    let Some(value) = read_line(prompt) else {
        println!("wake: cancelled");
        return Ok(());
    };
    let value = value.trim();
    if value.is_empty() {
        println!("wake: cancelled");
        return Ok(());
    }
    match flag {
        Some(flag) => commands::start(&start_args(no_display, &[flag, value])),
        None => commands::start(&start_args(no_display, &[value])),
    }
}

fn start_args(no_display: bool, parts: &[&str]) -> Vec<String> {
    let mut args = Vec::with_capacity(parts.len() + usize::from(no_display));
    if no_display {
        args.push("--no-display".to_string());
    }
    args.extend(parts.iter().map(|part| (*part).to_string()));
    args
}

fn build_menu(active: bool) -> Vec<Item> {
    let mut items = if active {
        vec![
            Item::new("Show status", "view session details", Action::ToggleDetail),
            Item::new("Stop session", "end the active session", Action::Stop),
        ]
    } else {
        vec![
            Item::new("Indefinite", "stay awake forever", Action::StartIndefinite),
            Item::new(
                "For a duration…",
                "1h, 30m, 1h30m, 90s",
                Action::Ask("Duration (e.g. 1h30m, 90s)", None),
            ),
            Item::new(
                "Until clock time…",
                "stay awake until HH:MM",
                Action::Ask("Until clock time (HH:MM)", Some("--until")),
            ),
            Item::new(
                "Until battery %…",
                "until charge hits N%",
                Action::Ask("Target battery percent (1-100)", Some("--until-charge")),
            ),
            Item::new(
                "While app running…",
                "watch a running app/process",
                Action::Ask("App/process name", Some("--while-app")),
            ),
            Item::new(
                "While PID alive…",
                "watch a specific process id",
                Action::Ask("PID to watch", Some("--while-pid")),
            ),
        ]
    };
    items.extend([
        Item::separator(),
        Item::new("Quit", "exit without changes", Action::Quit),
    ]);
    items
}

fn next(items: &[Item], cur: usize) -> usize {
    adjacent(items, cur, 1)
}

fn prev(items: &[Item], cur: usize) -> usize {
    adjacent(items, cur, -1)
}

fn adjacent(items: &[Item], current: usize, direction: isize) -> usize {
    let len = items.len() as isize;
    (1..=len)
        .map(|step| (current as isize + direction * step).rem_euclid(len) as usize)
        .find(|&index| items[index].action.is_some())
        .unwrap_or(current)
}

fn append_detail(sb: &mut String, s: &Session, now: chrono::DateTime<Utc>) {
    let started = s.started_at;
    let elapsed = (now - started).num_seconds();
    let remaining = match s.ends_at {
        None => "-".to_string(),
        Some(e) => commands::pretty_duration((e - now).num_seconds().max(0)),
    };
    writeln!(sb, "    mode      : {}", s.spec.mode.label()).ok();
    writeln!(
        sb,
        "    trigger   : {} ({})",
        s.spec.trigger.label(),
        s.spec.trigger.detail()
    )
    .ok();
    writeln!(
        sb,
        "    started   : {} ({} ago)",
        started.with_timezone(&Local).format("%H:%M:%S"),
        commands::pretty_duration(elapsed)
    )
    .ok();
    writeln!(sb, "    remaining : {remaining}").ok();
}

fn ask_yes_no(prompt: &str) -> bool {
    let theme = Theme::current();
    print!("\n  {} {} ", theme.color(prompt, 87), theme.dim("[y/N]"));
    std::io::stdout().flush().ok();
    read_stdin_line().is_some_and(|answer| matches!(answer.trim().chars().next(), Some('y' | 'Y')))
}

fn read_line(prompt: &str) -> Option<String> {
    print!("\n  {} ", Theme::current().color(format!("{prompt}:"), 87));
    std::io::stdout().flush().ok();
    read_stdin_line()
}

fn read_stdin_line() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_restore_is_idempotent() {
        let mut terminal = Terminal {
            active: true,
            raw: false,
        };
        let mut output = Vec::new();

        terminal.leave_to(&mut output);
        let restored = output.clone();
        terminal.leave_to(&mut output);

        assert!(!restored.is_empty());
        assert_eq!(output, restored);
    }

    #[test]
    fn menus_keep_every_user_action() {
        let labels = |active| {
            build_menu(active)
                .into_iter()
                .filter(|item| item.action.is_some())
                .map(|item| item.label)
                .collect::<Vec<_>>()
        };

        assert_eq!(labels(true), ["Show status", "Stop session", "Quit"]);
        assert_eq!(
            labels(false),
            [
                "Indefinite",
                "For a duration…",
                "Until clock time…",
                "Until battery %…",
                "While app running…",
                "While PID alive…",
                "Quit",
            ]
        );
    }

    #[test]
    fn display_sleep_flag_precedes_action_arguments() {
        assert_eq!(
            start_args(true, &["--until", "12:30"]),
            ["--no-display", "--until", "12:30"]
        );
        assert_eq!(start_args(false, &["1h"]), ["1h"]);
    }

    #[test]
    fn navigation_skips_separator_and_wraps() {
        let items = build_menu(true);

        assert_eq!(next(&items, 1), 3);
        assert_eq!(next(&items, 3), 0);
        assert_eq!(prev(&items, 0), 3);
        assert_eq!(prev(&items, 3), 1);
    }
}
