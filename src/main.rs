mod commands;
mod durations;
mod error;
mod lid;
mod platform;
mod run;
mod session;
mod supervisor;
mod sysutil;

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
            #[cfg(windows)]
            "__lid_restore__" => return lid::run_restore(&args[1..]),
            // v0.1.1 in-place sessions may still invoke this elevated restore command.
            #[cfg(windows)]
            "__set_lid__" => return lid::run_set_lid(&args[1..]),
            _ => {}
        }
    }

    match args.first().map(String::as_str) {
        Some("-h" | "--help" | "help") => {
            print_help();
            Ok(())
        }
        Some("-v" | "--version" | "version") => {
            println!("wake {VERSION}");
            Ok(())
        }
        Some("status") => commands::status(),
        Some("stop") => commands::stop(),
        _ => commands::start(args),
    }
}

pub(crate) fn print_help() {
    println!(
        r"wake - keep your machine awake from the CLI

usage:
  wake [OPTIONS] [TRIGGER]
  wake <COMMAND>

commands:
  status                     show the current session
  stop                       request a graceful stop and recovery
  help, -h, --help           show this message
  version, -v, --version     print the version

triggers (choose at most one; omitted means indefinite):
  <duration>                 stay awake for a duration
  forever | indefinite      stay awake indefinitely
  -t, --for DURATION        stay awake for a duration
  --until HH:MM             stay awake until the next local clock time
  --until-charge N          stay awake until battery reaches N% (1-100)
  --while-pid PID           stay awake while the observed PID identity exists
  --while-app NAME          stay awake while a matching process identity exists

options:
  --no-display              prevent system sleep only; allow display sleep
  --even-lid                request closed-lid operation

duration syntax:
  90s, 5m, 1h, 1h30m, 2h45m30s, 1d, or plain seconds (3600)
  maximum: 30d

platform requirements:
  macOS    uses caffeinate; closed-lid operation needs interactive sudo and a
           root-owned protected install with no extended ACL
  Linux    uses systemd-inhibit, systemd-logind, and GNU tail with --pid;
           closed-lid operation starts only if logind grants a
           handle-lid-switch inhibitor
  Windows  uses native power requests; closed-lid operation needs UAC elevation
           by the same Windows account
  macOS or Windows recovery may request the same elevation again

exit codes:
  0 success, 1 runtime or recovery error, 2 usage error

private state directory:
  Unix: $XDG_STATE_HOME/wake or ~/.local/state/wake
  Windows: %LOCALAPPDATA%\wake
  Override: WAKE_STATE_DIR"
    );
}
