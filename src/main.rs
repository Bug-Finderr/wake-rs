//! wake - keep your machine awake from the CLI. Rust port of wake-cli (binary name: `wake`).

mod commands;
mod durations;
mod error;
mod platform;
mod session;
mod supervisor;
mod sysutil;

#[cfg(unix)]
mod interactive;

use error::AppError;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = dispatch(&args) {
        eprintln!("wake: {}", e.message());
        if matches!(e, AppError::Usage(_)) {
            eprintln!("try 'wake --help'");
        }
        std::process::exit(e.exit_code());
    }
}

fn dispatch(args: &[String]) -> Result<(), AppError> {
    if let Some(first) = args.first() {
        match first.as_str() {
            "-h" | "--help" | "help" => {
                print_help();
                return Ok(());
            }
            "-v" | "--version" | "version" => {
                println!("wake {VERSION}");
                return Ok(());
            }
            "status" => return commands::status(),
            "stop" => return commands::stop(),
            "forever" | "indefinite" => return commands::start_forever(args),
            "__supervise_charge__" => return supervisor::run_charge(args),
            "__supervise_lid__" => return supervisor::run_lid(args),
            #[cfg(windows)]
            "__set_lid__" => return set_lid(&args[1..]),
            _ => return commands::start(args),
        }
    }

    commands::recover_stale_lid_session_foreground()?;

    #[cfg(unix)]
    {
        if commands::is_console() && platform::supports_interactive() {
            return interactive::run();
        }
    }
    commands::start(&[])
}

/// Hidden, Windows-only helper meant to run elevated: set the power-plan lid action to `<ac> <dc>`.
/// Success is a silent exit 0; failure prints to stderr (via `main`) and exits non-zero.
#[cfg(windows)]
fn set_lid(args: &[String]) -> Result<(), AppError> {
    fn parse(raw: &str) -> Result<u32, AppError> {
        match raw.trim().parse::<u32>() {
            Ok(v @ 0..=3) => Ok(v),
            _ => Err(AppError::fail(
                "__set_lid__ expects two lid actions in 0..=3",
            )),
        }
    }
    let (Some(ac), Some(dc)) = (args.first(), args.get(1)) else {
        return Err(AppError::fail("__set_lid__ expects <ac> <dc>"));
    };
    platform::write_lid_action(parse(ac)?, parse(dc)?)
}

fn print_help() {
    println!(
        r#"wake - keep your machine awake from the CLI

platforms:
  macOS uses caffeinate; Linux uses systemd-inhibit and requires systemd;
  Windows uses PowerShell + SetThreadExecutionState
  note: closing the lid still sleeps the mac unless you use --even-lid

interactive:
  wake                       open the picker on macOS/Linux; on Windows, start indefinitely

direct:
  wake forever               stay awake indefinitely (no menu)
  wake <duration>            e.g. wake 1h, wake 30m, wake 1h30m, wake 90s
  wake -t <duration>         same as above with explicit flag
  wake --until HH:MM         stay awake until clock time
  wake --until-charge N      stay awake until battery hits N% (1-100)
  wake --while-pid PID       stay awake while PID is running
  wake --while-app NAME      stay awake while named app/process is running
  wake --no-display          prevent system sleep only, allow display sleep
  wake --even-lid            stay awake with the lid closed (macOS uses sudo; Windows sets
                             the lid-close action to Do Nothing)
  wake status                show current session
  wake stop                  end current session
  wake version               print version
  wake help                  this message

duration syntax:
  90s, 5m, 1h, 1h30m, 2h45m30s, 1d, or plain seconds (3600)
  maximum: 30d

state file:
  ~/.local/state/wake/session.properties (override dir with WAKE_STATE_DIR)"#
    );
}
