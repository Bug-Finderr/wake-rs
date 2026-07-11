mod commands;
mod durations;
mod error;
mod lid;
mod platform;
mod run;
mod session;
mod supervisor;
mod sysutil;

#[cfg(unix)]
mod interactive;

use error::AppError;

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

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
            "__supervise__" => return supervisor::run(&args[1..]),
            #[cfg(any(windows, target_os = "macos"))]
            "__lid_watchdog__" => return lid::run_watchdog(&args[1..]),
            #[cfg(any(windows, target_os = "macos"))]
            "__lid_restore__" => return lid::run_restore(&args[1..]),
            _ => {}
        }
    }

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
            _ => return commands::start(args),
        }
    }

    #[cfg(unix)]
    {
        if commands::is_console() && platform::supports_interactive() {
            return interactive::run();
        }
    }
    commands::start(&[])
}

pub(crate) fn print_help() {
    println!(
        r"wake - keep your machine awake from the CLI

platforms:
  macOS uses caffeinate; Linux uses systemd-inhibit and requires systemd;
  Windows uses native power requests
  note: closing the lid still sleeps the mac unless you use --even-lid

interactive:
  wake                       open the picker on macOS/Linux; on Windows, start indefinitely
                             (macOS/Linux: a non-interactive or piped 'wake' starts indefinitely)

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
  wake version, -v           print version
  wake help, -h              this message

duration syntax:
  90s, 5m, 1h, 1h30m, 2h45m30s, 1d, or plain seconds (3600)
  maximum: 30d

exit codes:
  2 usage, 1 error

state file:
  ~/.local/state/wake/session.json (override dir with WAKE_STATE_DIR)
  Windows: %LOCALAPPDATA%\wake\session.json"
    );
}
