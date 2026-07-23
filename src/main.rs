//! wake - keep your machine awake from the CLI.

mod commands;
mod durations;
mod error;
mod platform;
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
    let internal = args.first().is_some_and(|arg| arg.starts_with("__"));
    if !internal && args.len() > 1 && args.iter().any(|arg| is_help_or_version(arg)) {
        return Err(AppError::usage("help and version must be used alone"));
    }
    let Some((first, rest)) = args.split_first() else {
        return commands::start(args);
    };
    match first.as_str() {
        "-h" | "--help" | "help" => {
            print_help();
            Ok(())
        }
        "-v" | "--version" | "version" => {
            println!("wake {VERSION}");
            Ok(())
        }
        "status" => {
            reject_trailing(first, rest)?;
            commands::status()
        }
        "stop" => {
            reject_trailing(first, rest)?;
            commands::stop()
        }
        #[cfg(not(windows))]
        "__supervise_charge__" => supervisor::run_charge(args),
        #[cfg(not(windows))]
        "__supervise_lid__" => supervisor::run_lid(args),
        #[cfg(windows)]
        "__worker_windows__" => supervisor::run_worker(args),
        #[cfg(windows)]
        "__guard_windows__" => supervisor::run_guardian(args),
        _ => commands::start(args),
    }
}

fn is_help_or_version(arg: &str) -> bool {
    matches!(
        arg,
        "-h" | "--help" | "help" | "-v" | "--version" | "version"
    )
}

fn reject_trailing(command: &str, args: &[String]) -> Result<(), AppError> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(AppError::usage(format!(
            "{command} does not accept arguments"
        )))
    }
}

pub(crate) fn print_help() {
    println!(
        r#"wake - keep your machine awake from the CLI

platforms:
  macOS uses caffeinate; Linux uses systemd-inhibit and requires systemd;
  Windows uses native SetThreadExecutionState
  note: closing the lid still sleeps the mac unless you use --even-lid

usage:
  wake                       stay awake indefinitely
  wake forever               stay awake indefinitely
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
  ~/.local/state/wake/session.properties (override dir with WAKE_STATE_DIR)
  Windows: %LOCALAPPDATA%\wake\session.properties"#
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn fixed_commands_reject_trailing_args() {
        for values in [
            &["help", "extra"][..],
            &["version", "extra"],
            &["status", "extra"],
            &["stop", "extra"],
        ] {
            assert!(matches!(dispatch(&args(values)), Err(AppError::Usage(_))));
        }
    }

    #[test]
    fn start_help_and_version_must_be_alone() {
        for values in [&["1h", "--help"][..], &["--no-display", "--version"]] {
            assert!(matches!(dispatch(&args(values)), Err(AppError::Usage(_))));
        }
    }
}
