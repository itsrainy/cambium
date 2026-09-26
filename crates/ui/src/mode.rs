//! What kind of output this invocation gets, decided once at startup.
//!
//! Nothing downstream asks the environment again: a verb that has been given
//! an [`OutputMode`] renders the same way from the first event to the last,
//! even if the terminal is resized or the variables change under it.

/// `--color`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

/// Which sink a run draws through.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A terminal: a live bottom line, and a finished phase rewritten in
    /// place.
    Stream,
    /// The same bytes without the live line, for a pipe or a CI log.
    Plain,
    /// One JSON object per event, on stderr.
    Jsonl,
    /// Nothing at all.
    Null,
}

/// The resolved shape of this run's progress output.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OutputMode {
    pub kind: Kind,
    /// A live bottom line is drawn. Only ever true for [`Kind::Stream`].
    pub live: bool,
    pub colour: bool,
    /// Measured once, so every row of a run lines up with every other.
    pub width: usize,
}

impl OutputMode {
    /// What a terminal gets when nothing is suppressing it.
    pub fn detect(quiet: bool, jsonl: bool, colour: ColorChoice) -> Self {
        Self::decide(quiet, jsonl, colour, &Env::read())
    }

    /// Whether text on stderr takes colour, by the rule the narration uses:
    /// `--color`, then `NO_COLOR`, `CLICOLOR`, `CLICOLOR_FORCE`, `TERM=dumb`
    /// and whether stderr is a terminal.
    ///
    /// For what is printed whatever [`Kind`] the run got: a diagnostic, or
    /// an error under `--quiet`. The narration's own `colour` is this and
    /// also off for the kinds that draw no text.
    pub fn stderr_colour(colour: ColorChoice) -> bool {
        colour_for(colour, &Env::read())
    }

    pub fn silent() -> Self {
        Self {
            kind: Kind::Null,
            live: false,
            colour: false,
            width: 80,
        }
    }

    fn decide(quiet: bool, jsonl: bool, colour: ColorChoice, env: &Env) -> Self {
        // `--quiet` means what the CLI has always said it means: nothing on
        // stderr when the run succeeds. `--format jsonl` means the progress
        // is for a program, so it is objects rather than lines.
        let kind = if quiet {
            Kind::Null
        } else if jsonl {
            Kind::Jsonl
        } else if env.tty && !env.ci && !env.dumb {
            Kind::Stream
        } else {
            Kind::Plain
        };
        let colour = colour_for(colour, env) && !matches!(kind, Kind::Jsonl | Kind::Null);
        Self {
            kind,
            live: matches!(kind, Kind::Stream),
            colour,
            width: crate::row::terminal_width(),
        }
    }
}

/// The one colour rule, for the narration and the diagnostics alike.
fn colour_for(colour: ColorChoice, env: &Env) -> bool {
    match colour {
        ColorChoice::Never => false,
        ColorChoice::Always => true,
        // NO_COLOR and CLICOLOR=0 turn it off whatever the terminal is;
        // CLICOLOR_FORCE turns it on even down a pipe.
        ColorChoice::Auto => {
            !env.no_color && !env.clicolor_off && !env.dumb && (env.clicolor_force || env.tty)
        }
    }
}

/// The environment, read once.
#[derive(Copy, Clone, Debug, Default)]
struct Env {
    tty: bool,
    ci: bool,
    dumb: bool,
    no_color: bool,
    clicolor_off: bool,
    clicolor_force: bool,
}

impl Env {
    fn read() -> Self {
        use std::io::IsTerminal;
        let var = |k: &str| std::env::var(k).ok();
        Self {
            tty: std::io::stderr().is_terminal(),
            ci: std::env::var_os("CI").is_some_and(|v| !v.is_empty() && v != "0" && v != "false"),
            dumb: var("TERM").as_deref() == Some("dumb"),
            no_color: std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
            clicolor_off: var("CLICOLOR").as_deref() == Some("0"),
            clicolor_force: var("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tty() -> Env {
        Env {
            tty: true,
            ..Env::default()
        }
    }

    #[test]
    fn a_terminal_is_live_and_coloured_and_a_pipe_is_neither() {
        let m = OutputMode::decide(false, false, ColorChoice::Auto, &tty());
        assert_eq!(m.kind, Kind::Stream);
        assert!(m.live && m.colour);

        let m = OutputMode::decide(false, false, ColorChoice::Auto, &Env::default());
        assert_eq!(m.kind, Kind::Plain);
        assert!(!m.live && !m.colour);
    }

    #[test]
    fn ci_and_a_dumb_terminal_keep_the_rows_and_drop_the_live_line() {
        let m = OutputMode::decide(false, false, ColorChoice::Auto, &Env { ci: true, ..tty() });
        assert_eq!(m.kind, Kind::Plain);
        let m = OutputMode::decide(
            false,
            false,
            ColorChoice::Auto,
            &Env {
                dumb: true,
                ..tty()
            },
        );
        assert_eq!(m.kind, Kind::Plain);
        assert!(!m.colour, "a dumb terminal takes no escapes");
    }

    #[test]
    fn quiet_says_nothing_and_jsonl_says_it_in_objects() {
        assert_eq!(
            OutputMode::decide(true, false, ColorChoice::Always, &tty()).kind,
            Kind::Null
        );
        let m = OutputMode::decide(false, true, ColorChoice::Always, &tty());
        assert_eq!(m.kind, Kind::Jsonl);
        assert!(!m.colour, "a JSON object is never painted");
    }

    #[test]
    fn no_color_and_clicolor_decide_colour_and_force_beats_a_pipe() {
        let off = Env {
            no_color: true,
            ..tty()
        };
        assert!(!OutputMode::decide(false, false, ColorChoice::Auto, &off).colour);
        assert!(
            OutputMode::decide(false, false, ColorChoice::Always, &off).colour,
            "--color always is a caller asking for it on purpose"
        );
        let zero = Env {
            clicolor_off: true,
            ..tty()
        };
        assert!(!OutputMode::decide(false, false, ColorChoice::Auto, &zero).colour);
        let forced = Env {
            clicolor_force: true,
            ..Env::default()
        };
        let m = OutputMode::decide(false, false, ColorChoice::Auto, &forced);
        assert!(m.colour && m.kind == Kind::Plain);
    }

    /// The diagnostics' rule is the narration's, minus the kinds: an error
    /// printed under `--quiet` still colours on a terminal.
    #[test]
    fn stderr_colour_is_the_same_rule_whatever_the_kind() {
        let dumb = Env {
            dumb: true,
            ..tty()
        };
        assert!(!colour_for(ColorChoice::Auto, &dumb));
        let zero = Env {
            clicolor_off: true,
            ..tty()
        };
        assert!(!colour_for(ColorChoice::Auto, &zero));
        let forced = Env {
            clicolor_force: true,
            ..Env::default()
        };
        assert!(colour_for(ColorChoice::Auto, &forced));
        assert!(colour_for(ColorChoice::Auto, &tty()));
        assert!(!OutputMode::decide(true, false, ColorChoice::Auto, &tty()).colour);
    }
}
