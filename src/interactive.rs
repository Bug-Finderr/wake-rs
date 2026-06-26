//! Interactive picker (macOS/Linux). Raw-mode key handling via crossterm; ANSI rendering matches
//! the reference. Each selectable action delegates to `commands` so all the safety machinery runs.

use crate::commands;
use crate::error::Result;
use crate::session::{self, Session};
use chrono::{Local, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{BufRead, Write};

const ESC: &str = "\u{1B}";

fn style_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    match std::env::var("TERM") {
        Ok(t) => !t.trim().is_empty() && !t.eq_ignore_ascii_case("dumb"),
        Err(_) => false,
    }
}

fn style(code: &str) -> String {
    if style_enabled() {
        format!("{ESC}{code}")
    } else {
        String::new()
    }
}

enum Action {
    Noop,
    ToggleDetail,
    Quit,
    Stop,
    StartIndefinite,
    Ask(&'static str, Option<&'static str>),
}

struct Item {
    label: String,
    hint: String,
    action: Action,
    separator: bool,
    exit_after: bool,
}

impl Item {
    fn new(label: &str, hint: &str, action: Action, exit_after: bool) -> Self {
        Item {
            label: label.into(),
            hint: hint.into(),
            action,
            separator: false,
            exit_after,
        }
    }
    fn sep() -> Self {
        Item {
            label: String::new(),
            hint: String::new(),
            action: Action::Noop,
            separator: true,
            exit_after: false,
        }
    }
}

pub fn run() -> Result<()> {
    Picker::default().loop_()
}

#[derive(Default)]
struct Picker {
    no_display: bool,
    show_detail: bool,
    selected: usize,
    raw: bool,
}

impl Drop for Picker {
    fn drop(&mut self) {
        self.cleanup();
    }
}

impl Picker {
    fn loop_(&mut self) -> Result<()> {
        enable_raw_mode().ok();
        self.raw = true;
        print!("{}{}", alt_on(), hide_cursor());
        std::io::stdout().flush().ok();

        let existing = session::read_if_alive(false);
        let items = build_menu(&existing);
        self.selected = items.iter().position(|i| !i.separator).unwrap_or(0);

        loop {
            self.render(&items, &existing);
            match self.read_key()? {
                Key::Up => self.selected = prev(&items, self.selected),
                Key::Down => self.selected = next(&items, self.selected),
                Key::Toggle if existing.is_none() => self.no_display = !self.no_display,
                Key::Quit => {
                    self.cleanup();
                    return Ok(());
                }
                Key::Enter => {
                    let it = &items[self.selected];
                    if it.separator {
                        continue;
                    }
                    if it.exit_after {
                        self.cleanup();
                        return self.run_action(&it.action);
                    }
                    // in-place actions (toggle detail)
                    if let Action::ToggleDetail = it.action {
                        self.show_detail = !self.show_detail;
                    }
                }
                _ => {}
            }
        }
    }

    fn run_action(&self, action: &Action) -> Result<()> {
        match action {
            Action::Stop => commands::stop(),
            Action::StartIndefinite => self.start_indefinite(),
            Action::Ask(prompt, flag) => self.ask_and_start(prompt, *flag),
            _ => Ok(()),
        }
    }

    fn start_indefinite(&self) -> Result<()> {
        if crate::platform::supports_even_lid()
            && ask_yes_no("Keep awake with the lid closed too? (needs sudo)")
        {
            commands::start(&self.build_args(&["--even-lid"]))
        } else {
            commands::start(&self.build_args(&[]))
        }
    }

    fn ask_and_start(&self, prompt: &str, flag: Option<&str>) -> Result<()> {
        let Some(v) = read_line(prompt) else {
            println!("wake: cancelled");
            return Ok(());
        };
        let v = v.trim().to_string();
        if v.is_empty() {
            println!("wake: cancelled");
            return Ok(());
        }
        match flag {
            None => commands::start(&self.build_args(&[&v])),
            Some(f) => commands::start(&self.build_args(&[f, &v])),
        }
    }

    fn build_args(&self, parts: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        if self.no_display {
            out.push("--no-display".to_string());
        }
        out.extend(parts.iter().map(|s| s.to_string()));
        out
    }

    fn render(&self, items: &[Item], existing: &Option<Session>) {
        let mut sb = String::with_capacity(2048);
        sb.push_str(&clear());
        sb.push_str(&home());
        sb.push('\n');
        sb.push_str(&format!("  {}{}☕ wake{}", bold(), fg_cyan(), reset()));
        sb.push_str(&format!(
            "  {}- keep your machine awake{}\n\n",
            dim(),
            reset()
        ));

        if let Some(s) = existing {
            sb.push_str(&format!(
                "  {}● active{}   {} {}",
                fg_yellow(),
                reset(),
                s.trigger,
                s.detail
            ));
            if let Some(end) = s.ends_at {
                let rem = (end - Utc::now()).num_seconds().max(0);
                sb.push_str(&format!(
                    "   {}({} left){}",
                    dim(),
                    commands::pretty_duration(rem),
                    reset()
                ));
            }
            sb.push('\n');
            if self.show_detail {
                append_detail(&mut sb, s);
            }
            sb.push('\n');
        } else {
            sb.push_str(&format!(
                "  {}○ no active session{}\n\n",
                fg_grey(),
                reset()
            ));
        }

        for (i, it) in items.iter().enumerate() {
            if it.separator {
                sb.push_str(&format!(
                    "    {}─────────────────────────{}\n",
                    dim(),
                    reset()
                ));
                continue;
            }
            if i == self.selected {
                sb.push_str(&format!(
                    "  {}▸ {}{}{}",
                    fg_pink(),
                    bold(),
                    it.label,
                    reset()
                ));
                if !it.hint.is_empty() {
                    sb.push_str(&format!("   {}{}{}", dim(), it.hint, reset()));
                }
            } else {
                sb.push_str(&format!("    {}", it.label));
            }
            sb.push('\n');
        }

        sb.push_str(&format!("\n  {}↑↓/jk navigate · ↵ select", dim()));
        if existing.is_none() {
            sb.push_str(&format!(
                " · d display-sleep [{}]",
                if self.no_display { "ON" } else { "off" }
            ));
        }
        sb.push_str(&format!(" · q quit{}\n", reset()));

        print!("{sb}");
        std::io::stdout().flush().ok();
    }

    fn read_key(&self) -> Result<Key> {
        loop {
            match event::read().map_err(|e| crate::error::AppError::fail(e.to_string()))? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    return Ok(match k.code {
                        KeyCode::Up => Key::Up,
                        KeyCode::Down => Key::Down,
                        KeyCode::Char('k') => Key::Up,
                        KeyCode::Char('j') => Key::Down,
                        KeyCode::Char('d') | KeyCode::Char('D') => Key::Toggle,
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            Key::Quit
                        }
                        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => Key::Quit,
                        KeyCode::Enter => Key::Enter,
                        _ => Key::Other,
                    });
                }
                _ => continue,
            }
        }
    }

    fn cleanup(&mut self) {
        print!("{}{}{}", show_cursor(), alt_off(), reset());
        std::io::stdout().flush().ok();
        if self.raw {
            disable_raw_mode().ok();
            self.raw = false;
        }
    }
}

enum Key {
    Up,
    Down,
    Toggle,
    Quit,
    Enter,
    Other,
}

fn build_menu(existing: &Option<Session>) -> Vec<Item> {
    let mut items = Vec::new();
    if existing.is_some() {
        items.push(Item::new(
            "Show status",
            "view session details",
            Action::ToggleDetail,
            false,
        ));
        items.push(Item::new(
            "Stop session",
            "end the active session",
            Action::Stop,
            true,
        ));
    } else {
        items.push(Item::new(
            "Indefinite",
            "stay awake forever",
            Action::StartIndefinite,
            true,
        ));
        items.push(Item::new(
            "For a duration…",
            "1h, 30m, 1h30m, 90s",
            Action::Ask("Duration (e.g. 1h30m, 90s)", None),
            true,
        ));
        items.push(Item::new(
            "Until clock time…",
            "stay awake until HH:MM",
            Action::Ask("Until clock time (HH:MM)", Some("--until")),
            true,
        ));
        items.push(Item::new(
            "Until battery %…",
            "until charge hits N%",
            Action::Ask("Target battery percent (1-100)", Some("--until-charge")),
            true,
        ));
        items.push(Item::new(
            "While app running…",
            "watch a running app/process",
            Action::Ask("App/process name", Some("--while-app")),
            true,
        ));
        items.push(Item::new(
            "While PID alive…",
            "watch a specific process id",
            Action::Ask("PID to watch", Some("--while-pid")),
            true,
        ));
    }
    items.push(Item::sep());
    items.push(Item::new(
        "Quit",
        "exit without changes",
        Action::Quit,
        true,
    ));
    items
}

fn next(items: &[Item], cur: usize) -> usize {
    let n = items.len();
    for step in 1..=n {
        let idx = (cur + step) % n;
        if !items[idx].separator {
            return idx;
        }
    }
    cur
}

fn prev(items: &[Item], cur: usize) -> usize {
    let n = items.len();
    for step in 1..=n {
        let idx = (cur + n - (step % n)) % n;
        if !items[idx].separator {
            return idx;
        }
    }
    cur
}

fn append_detail(sb: &mut String, s: &Session) {
    let now = Utc::now();
    let started = s.started_at.unwrap_or(now);
    let elapsed = (now - started).num_seconds();
    let remaining = match s.ends_at {
        None => "-".to_string(),
        Some(e) => commands::pretty_duration((e - now).num_seconds().max(0)),
    };
    sb.push_str(&format!("    mode      : {}\n", s.mode));
    sb.push_str(&format!("    trigger   : {} ({})\n", s.trigger, s.detail));
    sb.push_str(&format!(
        "    started   : {} ({} ago)\n",
        started.with_timezone(&Local).format("%H:%M:%S"),
        commands::pretty_duration(elapsed)
    ));
    sb.push_str(&format!("    remaining : {remaining}\n"));
}

fn ask_yes_no(prompt: &str) -> bool {
    print!(
        "\n  {}{}{} {}[y/N]{} ",
        fg_cyan(),
        prompt,
        reset(),
        dim(),
        reset()
    );
    std::io::stdout().flush().ok();
    match read_stdin_line() {
        Some(answer) => {
            let a = answer.trim();
            matches!(a.chars().next(), Some('y') | Some('Y'))
        }
        None => false,
    }
}

fn read_line(prompt: &str) -> Option<String> {
    print!("\n  {}{}:{} ", fg_cyan(), prompt, reset());
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

// ANSI helpers (computed lazily so NO_COLOR/TERM are honored at runtime).
fn alt_on() -> String {
    format!("{ESC}[?1049h")
}
fn alt_off() -> String {
    format!("{ESC}[?1049l")
}
fn clear() -> String {
    format!("{ESC}[2J")
}
fn home() -> String {
    format!("{ESC}[H")
}
fn hide_cursor() -> String {
    format!("{ESC}[?25l")
}
fn show_cursor() -> String {
    format!("{ESC}[?25h")
}
fn reset() -> String {
    style("[0m")
}
fn bold() -> String {
    style("[1m")
}
fn dim() -> String {
    style("[2m")
}
fn fg_cyan() -> String {
    style("[38;5;87m")
}
fn fg_yellow() -> String {
    style("[38;5;220m")
}
fn fg_grey() -> String {
    style("[38;5;245m")
}
fn fg_pink() -> String {
    style("[38;5;213m")
}
