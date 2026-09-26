//! Rendering diagnostics, in the `rustc` shape `docs/CLI.md` settles on.
//!
//! ```text
//! warning[CAMBI-W004]: .bazelrc changed and the run went global
//!   --> .bazelrc:14
//!   = help: add it to [bazelrc].allowlist_extra if it cannot change what `bazel query` loads
//! ```
//!
//! No source excerpt with carets: the header, the location and the help line
//! are what a caller acts on, and the excerpt would mean reading the file back
//! at the revision the diagnostic is about.
//!
//! Diagnostics are stderr's business. Results go to stdout, always, so a
//! pipeline that reads stdout is never handed a warning it has to filter out.

use cambium_engine::{Code, Diagnostic, Level};

use crate::Format;

/// How this invocation renders diagnostics.
#[derive(Copy, Clone, Debug)]
pub struct Renderer {
    pub format: Format,
    /// `--quiet`: nothing on stderr on success. An error still speaks, because
    /// an exit code with no message is not a diagnosis.
    pub quiet: bool,
    /// Resolved from `--color` and the environment by the narration's rule
    /// ([`cambium_ui::OutputMode::stderr_colour`]).
    pub color: bool,
}

impl Renderer {
    /// One warning, on stderr, in whatever form this invocation asked for.
    ///
    /// Under `--format json` the diagnostics of `cambi affected` live in the
    /// document instead and never come through here; a command whose document
    /// has no `diagnostics` field prints them here as JSON objects, so a
    /// machine reader sees the same fields either way.
    pub fn warn(&self, d: &Diagnostic) {
        if self.quiet {
            return;
        }
        eprintln!("{}", self.format_one(d));
    }

    /// An error, on stderr. Not suppressed by `--quiet`.
    pub fn error(&self, d: &Diagnostic) {
        eprintln!("{}", self.format_one(d));
    }

    /// A run's diagnostics, for a verb whose document carries them.
    ///
    /// Under `--format json` they are already in the document and nothing is
    /// printed; every other form puts them on stderr, where they cannot be
    /// mistaken for the answer, except under `jsonl`, which streams them to stdout
    /// as `diagnostic` events ahead of the `result`, so a watcher sees them as
    /// the run raised them rather than only at the end.
    pub fn report(&self, diagnostics: &[Diagnostic]) -> anyhow::Result<()> {
        match self.format {
            Format::Json => {}
            Format::Jsonl => {
                if !self.quiet {
                    for d in diagnostics {
                        println!("{}", crate::cmd::tagged_event("diagnostic", d)?);
                    }
                }
            }
            _ => diagnostics.iter().for_each(|d| self.warn(d)),
        }
        Ok(())
    }

    fn format_one(&self, d: &Diagnostic) -> String {
        match self.format {
            Format::Json | Format::Jsonl => serde_json::to_string(d)
                .unwrap_or_else(|e| format!(r#"{{"code":"CAMBI-E001","message":"{e}"}}"#)),
            _ => render_human(d, self.color),
        }
    }
}

/// ANSI, when colour is on: bold yellow for a `warning[CODE]` header, bold red
/// for an `error[CODE]` one, and the message in bold. The location and the help
/// line stay plain: a diagnostic is read, not decorated.
pub fn render_human(d: &Diagnostic, color: bool) -> String {
    let (bold, reset, tint) = if color {
        (
            "\x1b[1m",
            "\x1b[0m",
            match d.level {
                Level::Warning => "\x1b[1;33m",
                Level::Error => "\x1b[1;31m",
            },
        )
    } else {
        ("", "", "")
    };

    let mut out = format!(
        "{tint}{}[{}]{reset}{bold}: {}{reset}",
        d.level, d.code, d.message
    );
    if let Some(l) = &d.location {
        match l.line {
            Some(n) => out.push_str(&format!("\n  --> {}:{n}", l.path)),
            None => out.push_str(&format!("\n  --> {}", l.path)),
        }
    }
    if let Some(h) = &d.help {
        out.push_str(&format!("\n  = help: {h}"));
    }
    out
}

/// A usage error: the command line does not make sense. Its own type so the
/// top level can give it `CAMBI-E013` rather than the generic code.
#[derive(Debug)]
pub struct UsageError {
    pub message: String,
    /// What to type instead, when there is something more specific to say
    /// than [`USAGE_HELP`].
    pub help: Option<String>,
}

/// The `help:` line of a usage error that has nothing more specific to say,
/// which is what `docs/DIAGNOSTICS.md` promises every `CAMBI-E013` carries.
pub const USAGE_HELP: &str = "`cambi <verb> --help` lists what each verb takes";

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UsageError {}

impl UsageError {
    /// A usage error ready to `?`: it only ever travels as an `anyhow::Error`,
    /// because nothing branches on the type beyond the code it is given.
    pub fn raised(message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(UsageError {
            message: message.into(),
            help: None,
        })
    }

    /// The same, with a `help:` line naming the fix.
    pub fn raised_with_help(message: impl Into<String>, help: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(UsageError {
            message: message.into(),
            help: Some(help.into()),
        })
    }
}

/// A failure the CLI diagnosed itself, with the code and `help:` line it
/// already knows. A revision missing from this clone is one: the engine would
/// meet it later as git's own error text, and could not name the spelling the
/// caller typed or the fetch that fixes it.
#[derive(Debug)]
pub struct Raised(pub Diagnostic);

impl std::fmt::Display for Raised {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.message)
    }
}

impl std::error::Error for Raised {}

/// Whatever went wrong, as a diagnostic.
///
/// An `EngineError` knows its own code; a usage error is `CAMBI-E013`; and
/// anything else (an index that will not open, a serde failure) is the
/// generic internal code, whose message is the error chain a caller would
/// otherwise have read off `cambium: {e:#}`.
pub fn of_error(e: &anyhow::Error) -> Diagnostic {
    if let Some(engine) = e.downcast_ref::<cambium_engine::EngineError>() {
        return engine.diagnostic();
    }
    if let Some(usage) = e.downcast_ref::<UsageError>() {
        return Diagnostic::error(Code::E013Usage, usage.message.clone())
            .with_help(usage.help.as_deref().unwrap_or(USAGE_HELP));
    }
    if let Some(Raised(d)) = e.downcast_ref::<Raised>() {
        return d.clone();
    }
    Diagnostic::error(Code::E001Internal, chain(e))
}

/// The error and its causes, joined the way `{e:#}` would, except that a
/// cause whose text the error above it already carries is not repeated.
/// `io error on {path}: {source}` names its source in its own message, and
/// `{e:#}` would print the same "No such file or directory" twice.
fn chain(e: &anyhow::Error) -> String {
    let mut out = String::new();
    let mut prev: Option<String> = None;
    for cause in e.chain() {
        let text = cause.to_string();
        if prev.as_deref().is_some_and(|p| p.contains(&text)) {
            continue;
        }
        if !out.is_empty() {
            out.push_str(": ");
        }
        out.push_str(&text);
        prev = Some(text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cambium_engine::Diagnostic;

    #[test]
    fn a_cause_the_message_already_names_is_not_repeated() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let e = anyhow::Error::from(cambium_engine::EngineError::IoAt {
            path: "/x".into(),
            source: io,
        });
        // `EngineError` has its own code; force the generic path.
        let e = anyhow::anyhow!("{e}").context("opening the index");
        assert_eq!(chain(&e), "opening the index: io error on /x: gone");
        let plain = anyhow::anyhow!("inner").context("outer");
        assert_eq!(chain(&plain), "outer: inner");
    }

    fn full() -> Diagnostic {
        Diagnostic::warning(
            Code::W004BazelrcGlobal,
            ".bazelrc changed and the run went global",
        )
        .at(".bazelrc", Some(14))
        .with_help(
            "add it to [bazelrc].allowlist_extra if it cannot change what `bazel query` loads",
        )
    }

    #[test]
    fn the_human_form_is_the_rustc_one() {
        assert_eq!(
            render_human(&full(), false),
            "warning[CAMBI-W004]: .bazelrc changed and the run went global\n  \
             --> .bazelrc:14\n  \
             = help: add it to [bazelrc].allowlist_extra if it cannot change what `bazel query` loads"
        );
    }

    #[test]
    fn a_location_without_a_line_prints_the_path_alone() {
        let d = Diagnostic::warning(Code::W009WorkspaceLevelBzl, "loaded outside any package")
            .at("tools/defs.bzl", None);
        assert_eq!(
            render_human(&d, false),
            "warning[CAMBI-W009]: loaded outside any package\n  --> tools/defs.bzl"
        );
    }

    #[test]
    fn a_bare_diagnostic_is_one_line() {
        let d = Diagnostic::error(Code::E008NoStore, "no index store is configured");
        assert_eq!(
            render_human(&d, false),
            "error[CAMBI-E008]: no index store is configured"
        );
    }

    #[test]
    fn colour_is_only_ever_on_when_asked_for() {
        assert!(!render_human(&full(), false).contains('\x1b'));
        assert!(render_human(&full(), true).contains("\x1b[1;33m"));
    }

    #[test]
    fn under_json_a_diagnostic_is_an_object_not_a_banner() {
        let r = Renderer {
            format: Format::Json,
            quiet: false,
            color: false,
        };
        let s = r.format_one(&full());
        let v: serde_json::Value = serde_json::from_str(&s).expect("one JSON object per line");
        assert_eq!(v["code"], "CAMBI-W004");
        assert_eq!(v["location"]["line"], 14);
    }

    #[test]
    fn an_engine_error_keeps_its_own_code() {
        let e = anyhow::Error::new(cambium_engine::EngineError::NoStore);
        let d = of_error(&e);
        assert_eq!(d.code, Code::E008NoStore);
        assert_eq!(d.level, Level::Error);
        assert!(d.help.is_some(), "NoStore names the key to set");
    }

    #[test]
    fn anything_else_is_the_generic_error_code_with_the_whole_chain() {
        let e = anyhow::Error::msg("inner").context("outer");
        let d = of_error(&e);
        assert_eq!(d.code, Code::E001Internal);
        assert!(d.message.contains("outer"), "{}", d.message);
        assert!(d.message.contains("inner"), "{}", d.message);
    }

    #[test]
    fn a_usage_error_is_its_own_code() {
        let d = of_error(&UsageError::raised("--format query is only for affected"));
        assert_eq!(d.code, Code::E013Usage);
    }

    /// `docs/DIAGNOSTICS.md` promises every usage error a `help:` line, and a
    /// usage error that knows the fix names it instead of the generic one.
    #[test]
    fn a_usage_error_always_has_a_help_line() {
        let d = of_error(&UsageError::raised("no such flag"));
        assert_eq!(d.help.as_deref(), Some(USAGE_HELP));
        let d = of_error(&UsageError::raised_with_help("no range", "pass a range"));
        assert_eq!(d.help.as_deref(), Some("pass a range"));
    }

    #[test]
    fn a_raised_diagnostic_is_passed_through_whole() {
        let d = Diagnostic::error(Code::E003Git, "`origin/main` is not here").with_help("fetch it");
        let e = anyhow::Error::new(Raised(d.clone()));
        assert_eq!(of_error(&e), d);
        assert_eq!(e.to_string(), "`origin/main` is not here");
    }
}
