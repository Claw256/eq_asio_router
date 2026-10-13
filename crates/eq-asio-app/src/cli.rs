//! Command-line flags (parsed by hand, no extra dependency).

pub const HELP: &str = "\
Headroom (GUI)

Usage: eq-asio-app [OPTIONS]

Options:
  --background   Start hidden in the system tray and auto-start the engine
                 (this is what the 'Run at login' entry uses)
  --minimized    Start hidden in the system tray (engine start follows the
                 'Auto-start engine' setting)
  --no-engine    Never start the engine automatically (testing)
  -h, --help     Show this help

The app runs in the tray; closing the window hides it (configurable). Use the tray
menu or the Quit button to exit. Only one GUI instance runs: launching it again shows
the running window.
";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// Hidden start + engine auto-start.
    pub background: bool,
    /// Hidden start only.
    pub minimized: bool,
    /// Suppress every automatic engine start (testing).
    pub no_engine: bool,
    pub help: bool,
    /// Unrecognised arguments (reported, otherwise ignored).
    pub unknown: Vec<String>,
}

impl Args {
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Args {
        let mut a = Args::default();
        for arg in args {
            match arg.to_ascii_lowercase().as_str() {
                "--background" => a.background = true,
                "--minimized" => a.minimized = true,
                "--no-engine" => a.no_engine = true,
                "-h" | "--help" | "/?" => a.help = true,
                _ => a.unknown.push(arg),
            }
        }
        a
    }

    /// Window starts hidden in the tray.
    pub fn start_hidden(&self, setting_start_minimized: bool) -> bool {
        self.background || self.minimized || setting_start_minimized
    }

    /// Engine starts at launch.
    pub fn start_engine(&self, setting_auto_start: bool) -> bool {
        !self.no_engine && (self.background || setting_auto_start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Args {
        Args::parse(a.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_flags() {
        assert_eq!(p(&[]), Args::default());
        assert!(p(&["--background"]).background);
        assert!(p(&["--MINIMIZED"]).minimized);
        assert!(p(&["--help"]).help && p(&["-h"]).help);
        let a = p(&["--bogus", "--no-engine"]);
        assert!(a.no_engine);
        assert_eq!(a.unknown, vec!["--bogus".to_string()]);
    }

    #[test]
    fn hidden_and_engine_rules() {
        let normal = p(&[]);
        assert!(!normal.start_hidden(false) && normal.start_hidden(true));
        assert!(!normal.start_engine(false) && normal.start_engine(true));
        let bg = p(&["--background"]);
        assert!(bg.start_hidden(false), "background hides regardless of the setting");
        assert!(bg.start_engine(false), "background starts the engine regardless of auto_start");
        let min = p(&["--minimized"]);
        assert!(min.start_hidden(false) && !min.start_engine(false));
        let test = p(&["--background", "--no-engine"]);
        assert!(test.start_hidden(false) && !test.start_engine(true));
    }
}
