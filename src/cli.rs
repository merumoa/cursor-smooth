//! Strict command-line parsing shared by every platform.
//!
//! Parsing is deliberately total and allocation-light: an unknown or legacy
//! option must be rejected before any hook, tap or suppression exists.

use std::path::PathBuf;
use std::time::Duration;

pub const USAGE: &str = "\
cursor-smooth 0.1.0
Low-CPU software cursor smoothing.

USAGE:
    cursor-smooth run [--duration <seconds>] [--report <new-file.tsv>] [--stop-event <name>]
                      [--experimental-macos]
    cursor-smooth doctor
    cursor-smooth --help | --version

COMMANDS:
    run       Start smoothing. Ctrl+Alt+M toggles, Ctrl+Alt+Esc disables (Windows).
    doctor    Report platform readiness without installing a suppressing hook.

OPTIONS:
    --duration <seconds>  Stop automatically after a positive number of seconds.
    --report <path>       Write a UTF-8 TSV performance report; the file must not exist.
    --stop-event <name>   Windows-only: stop cleanly when this named event is signalled
                          (the tray companion uses it instead of killing the process).
    --experimental-macos  Acknowledge the unverified macOS implementation.
";

pub const PROBE_USAGE: &str = "\
cursor-smooth-probe
Diagnostic companion for cursor-smooth.

USAGE:
    cursor-smooth-probe replay --self-check
    cursor-smooth-probe source-probe --seconds <n> [--allow-cursor-move]
    cursor-smooth-probe fault-check [--allow-cursor-move]
    cursor-smooth-probe benchmark --mode <idle|replay-output|physical> --seconds <n> --trials <n> \\
        [--target <cursor-smooth executable>] [--allow-cursor-move]
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    pub duration: Option<Duration>,
    pub report: Option<PathBuf>,
    pub stop_event: Option<String>,
    pub experimental_macos: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Run(RunOptions),
    Doctor,
    Help,
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Unknown(String),
    MissingValue(&'static str),
    InvalidValue(&'static str, String),
    Duplicate(&'static str),
}

impl ParseError {
    pub fn message(&self) -> String {
        match self {
            ParseError::Unknown(value) => format!("unknown argument: {value}"),
            ParseError::MissingValue(flag) => format!("{flag} requires a value"),
            ParseError::InvalidValue(flag, value) => format!("invalid value for {flag}: {value}"),
            ParseError::Duplicate(flag) => format!("{flag} was given more than once"),
        }
    }
}

/// Parses the application arguments (without the program name).
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command, ParseError> {
    let args: Vec<String> = args.into_iter().collect();
    match args.first().map(String::as_str) {
        None => Ok(Command::Help),
        Some("--help") | Some("-h") => {
            if let Some(extra) = args.get(1) {
                Err(ParseError::Unknown(extra.clone()))
            } else {
                Ok(Command::Help)
            }
        }
        Some("--version") | Some("-V") => {
            if let Some(extra) = args.get(1) {
                Err(ParseError::Unknown(extra.clone()))
            } else {
                Ok(Command::Version)
            }
        }
        Some("doctor") => {
            if let Some(extra) = args.get(1) {
                Err(ParseError::Unknown(extra.clone()))
            } else {
                Ok(Command::Doctor)
            }
        }
        Some("run") => parse_run(&args[1..]).map(Command::Run),
        Some(other) => Err(ParseError::Unknown(other.to_string())),
    }
}

fn parse_run(args: &[String]) -> Result<RunOptions, ParseError> {
    let mut options = RunOptions {
        duration: None,
        report: None,
        stop_event: None,
        experimental_macos: false,
    };
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        match argument {
            "--duration" => {
                if options.duration.is_some() {
                    return Err(ParseError::Duplicate("--duration"));
                }
                let value = args
                    .get(index + 1)
                    .ok_or(ParseError::MissingValue("--duration"))?;
                let seconds: u64 = value
                    .parse()
                    .map_err(|_| ParseError::InvalidValue("--duration", value.clone()))?;
                if seconds == 0 {
                    return Err(ParseError::InvalidValue("--duration", value.clone()));
                }
                options.duration = Some(Duration::from_secs(seconds));
                index += 2;
            }
            "--report" => {
                if options.report.is_some() {
                    return Err(ParseError::Duplicate("--report"));
                }
                let value = args
                    .get(index + 1)
                    .ok_or(ParseError::MissingValue("--report"))?;
                if value.is_empty() {
                    return Err(ParseError::InvalidValue("--report", value.clone()));
                }
                options.report = Some(PathBuf::from(value));
                index += 2;
            }
            "--stop-event" => {
                if options.stop_event.is_some() {
                    return Err(ParseError::Duplicate("--stop-event"));
                }
                let value = args
                    .get(index + 1)
                    .ok_or(ParseError::MissingValue("--stop-event"))?;
                if !valid_event_name(value) {
                    return Err(ParseError::InvalidValue("--stop-event", value.clone()));
                }
                options.stop_event = Some(value.clone());
                index += 2;
            }
            "--experimental-macos" => {
                if options.experimental_macos {
                    return Err(ParseError::Duplicate("--experimental-macos"));
                }
                options.experimental_macos = true;
                index += 1;
            }
            other => return Err(ParseError::Unknown(other.to_string())),
        }
    }
    Ok(options)
}

/// Named kernel events accept a short, path-free identifier; rejecting spaces and
/// separators keeps the tray hand-off unambiguous and avoids `\` or `:` prefixes.
fn valid_event_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 120
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn legacy_live_start_options_are_rejected() {
        for legacy in ["--simulate", "--hz", "--rate", "--test"] {
            let error = parse(args(&["run", legacy])).unwrap_err();
            assert_eq!(error, ParseError::Unknown(legacy.to_string()));
        }
    }

    #[test]
    fn stop_event_names_are_strict() {
        assert!(parse(args(&["run", "--stop-event"])).is_err());
        assert!(parse(args(&["run", "--stop-event", ""])).is_err());
        assert!(parse(args(&["run", "--stop-event", "a b"])).is_err());
        assert!(parse(args(&["run", "--stop-event", "Global\\x"])).is_err());
        assert_eq!(
            parse(args(&["run", "--stop-event", "cursor-smooth-tray.stop_1"])),
            Ok(Command::Run(RunOptions {
                duration: None,
                report: None,
                stop_event: Some("cursor-smooth-tray.stop_1".to_string()),
                experimental_macos: false,
            }))
        );
        assert!(parse(args(&["run", "--stop-event", "a", "--stop-event", "b"])).is_err());
    }

    #[test]
    fn duration_must_be_positive_and_present() {
        assert!(parse(args(&["run", "--duration"])).is_err());
        assert!(parse(args(&["run", "--duration", "0"])).is_err());
        assert!(parse(args(&["run", "--duration", "-1"])).is_err());
        assert_eq!(
            parse(args(&["run", "--duration", "60"])),
            Ok(Command::Run(RunOptions {
                duration: Some(Duration::from_secs(60)),
                report: None,
                stop_event: None,
                experimental_macos: false,
            }))
        );
    }

    #[test]
    fn duplicate_flags_are_rejected() {
        assert!(parse(args(&["run", "--duration", "1", "--duration", "2"])).is_err());
        assert!(parse(args(&["run", "--report", "a.tsv", "--report", "b.tsv"])).is_err());
    }

    #[test]
    fn doctor_takes_no_options() {
        assert_eq!(parse(args(&["doctor"])), Ok(Command::Doctor));
        assert!(parse(args(&["doctor", "--report", "x"])).is_err());
    }

    #[test]
    fn help_and_version_are_commands() {
        assert_eq!(parse(args(&["--help"])), Ok(Command::Help));
        assert_eq!(parse(args(&["--version"])), Ok(Command::Version));
        assert_eq!(parse(Vec::<String>::new()), Ok(Command::Help));
        assert!(parse(args(&["--help", "run"])).is_err());
    }

    #[test]
    fn unknown_commands_are_rejected() {
        assert!(parse(args(&["start"])).is_err());
        assert!(parse(args(&["--experimental-macos"])).is_err());
    }

    #[test]
    fn report_path_is_required_with_a_value() {
        assert!(parse(args(&["run", "--report"])).is_err());
        assert!(parse(args(&["run", "--report", ""])).is_err());
        let parsed = parse(args(&["run", "--report", "out.tsv"])).unwrap();
        assert!(matches!(
            parsed,
            Command::Run(RunOptions { report: Some(path), .. }) if path == *"out.tsv"
        ));
    }
}
