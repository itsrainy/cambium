//! The verbs, one module each, and what they share.
//!
//! `main` turns the command line into a [`Context`] and hands it to one
//! verb's `run`. What a new verb uses rather than writes again lives here:
//! [`emit`] and [`emit_json`] print a document the way `--format` asked,
//! [`tagged_event`] is the `jsonl` framing, [`two_dot`] reads a range, and
//! [`short`], [`plural`], [`duration`] and [`human_bytes`] are the units a
//! revision, a count, an age and a size are said in. `narrate` holds the
//! pieces of the stderr narration more than one verb draws. `cambi config`
//! has no module of its own: each of its three subcommands is one document,
//! printed here.

pub mod affected;
pub mod doctor;
pub mod explain;
pub mod index;
pub mod init;
pub mod inspect;
mod narrate;
pub mod plan;
pub mod profile;
pub mod status;

use std::path::PathBuf;

use anyhow::Result;
use cambium_engine::config::Overrides;
use cambium_engine::{Config, Engine};

pub struct Context {
    pub root: PathBuf,
    pub config: Option<PathBuf>,
    /// What the command line and the environment said, with the spelling to
    /// attribute each value to. The highest layer of the configuration
    /// stack, and the only way a flag reaches the engine or a probe.
    pub overrides: Overrides,
    /// Where diagnostics go, and in what shape. It holds the global
    /// `--format`, which [`Context::format`] reads for every verb.
    pub render: crate::diag::Renderer,
    /// How a long run narrates itself on stderr: decided once, at startup,
    /// from `--quiet`, `--format`, `--color` and the terminal.
    pub ui: cambium_ui::OutputMode,
}

impl Context {
    /// The global `--format`.
    pub fn format(&self) -> crate::Format {
        self.render.format
    }

    pub fn engine(&self) -> Result<Engine> {
        Ok(Engine::open_with(
            &self.root,
            self.config.as_deref(),
            &self.overrides,
        )?)
    }

    pub fn config(&self) -> Result<Config> {
        Ok(self.layered()?.config)
    }

    /// The whole configuration stack with the origin of every effective value:
    /// defaults, `cambium.toml`, `cambium.local.toml`, then the flags and
    /// environment variables this invocation carries.
    ///
    /// Rooted at the workspace when there is one and at the working directory
    /// when there is not, so `cambi config explain` answers outside a
    /// workspace too.
    pub fn layered(&self) -> Result<cambium_engine::config::Layered> {
        let root =
            cambium_engine::bazel_workspace_root(&self.root).unwrap_or_else(|| self.root.clone());
        // Through `EngineError` so a config failure keeps its own diagnostic
        // code (`CAMBI-E014` for a misplaced local key) rather than arriving at
        // the top level as an anonymous internal error.
        Ok(
            Config::load_layered(&root, self.config.as_deref(), &self.overrides)
                .map_err(cambium_engine::EngineError::from)?,
        )
    }

    /// What `status`, `doctor` and `init` probe, from this invocation's globals.
    pub fn probe_options(&self) -> cambium_engine::probe::Options {
        cambium_engine::probe::Options {
            start: self.root.clone(),
            config_path: self.config.clone(),
            overrides: self.overrides.clone(),
            run_bazel: false,
            store_round_trip: false,
        }
    }
}

/// `cambi config show|explain|hash`.
#[derive(clap::Subcommand)]
pub enum ConfigArgs {
    /// Print the effective configuration and its hash.
    Show,
    /// Print every effective value and where it came from.
    Explain,
    /// Print the config hash alone, as hex.
    Hash,
}

pub fn config(ctx: &Context, args: ConfigArgs) -> Result<u8> {
    match args {
        ConfigArgs::Show => config_show(ctx),
        ConfigArgs::Explain => config_explain(ctx),
        ConfigArgs::Hash => {
            // One line, no formatting of its own: this exists so a script can
            // compare it against an index's, and `--format json` wraps it in
            // the same document `show` prints.
            let cfg = ctx.config()?;
            let hash = hex(&cfg.hash());
            let doc = serde_json::json!({
                "schema": cambium_engine::report::SCHEMA_VERSION,
                "config_hash": hash,
            });
            emit(&doc, ctx.format().data("cambi config hash")?, |_| {
                println!("{hash}")
            })?;
            Ok(crate::EXIT_OK)
        }
    }
}

pub fn config_show(ctx: &Context) -> Result<u8> {
    let cfg = ctx.config()?;
    // `show` is the file, not the attribution: the JSON forms print the
    // canonical TOML as one string field beside the hash every index
    // carries, and `explain` is the document with a field per value.
    let doc = serde_json::json!({
        "schema": cambium_engine::report::SCHEMA_VERSION,
        "config_hash": hex(&cfg.hash()),
        "toml": cfg.to_toml(),
    });
    emit(&doc, ctx.format().data("cambi config show")?, |_| {
        print!("{}", cfg.to_toml());
        println!("\n# config_hash = {}", hex(&cfg.hash()));
    })?;
    Ok(crate::EXIT_OK)
}

/// `cambi config explain`: one line per leaf key, with the layer that decided
/// it.
///
/// The question it answers is "why is this value this value", which on a
/// machine with a `cambium.local.toml`, a `--cache-dir` and a checked-in
/// `cambium.toml` is not guessable from any one of them.
pub fn config_explain(ctx: &Context) -> Result<u8> {
    let layered = ctx.layered()?;
    for d in &layered.diagnostics {
        ctx.render.warn(d);
    }
    let values = layered.explain();
    let doc = serde_json::json!({
        "schema": cambium_engine::report::SCHEMA_VERSION,
        "config_hash": hex(&layered.config.hash()),
        "config_file": layered.path,
        "local_file": layered.local_exists().then(|| layered.local_path.clone()),
        "values": values,
    });
    emit(&doc, ctx.format().data("cambi config explain")?, |_| {
        let width = values.iter().map(|v| v.key.len()).max().unwrap_or(0);
        for v in &values {
            println!(
                "{:<width$} = {}  ({})",
                v.key,
                v.value,
                v.origin_label,
                width = width
            );
        }
        println!();
        println!("# config_hash = {}", hex(&layered.config.hash()));
    })?;
    Ok(crate::EXIT_OK)
}

/// Prints one answer the way `--format` asked, for every verb that answers
/// with a document: nothing under `null`, `human` through the text renderer
/// (which sees the same value the JSON forms serialise), one pretty document
/// under `json`, one `result` event line under `jsonl`.
pub fn emit<T: serde::Serialize>(
    value: &T,
    fmt: Option<crate::DataFormat>,
    human: impl FnOnce(&T),
) -> Result<()> {
    match fmt {
        None => {}
        Some(crate::DataFormat::Text) => human(value),
        Some(fmt) => emit_json(value, fmt)?,
    }
    Ok(())
}

/// The JSON half of [`emit`], for a verb whose human form is not a rendering
/// of the document (`affected`, whose human output is the label list).
pub fn emit_json<T: serde::Serialize>(value: &T, fmt: crate::DataFormat) -> Result<()> {
    match fmt {
        crate::DataFormat::Jsonl => println!("{}", tagged_event("result", value)?),
        crate::DataFormat::Json | crate::DataFormat::Text => {
            println!("{}", serde_json::to_string_pretty(value)?)
        }
    }
    Ok(())
}

/// One `jsonl` line: `"event": kind` in front of the value's own fields, or
/// in front of the value itself, under `kind`, when it is not an object. The
/// framing a watcher parses, for the `result` at the end of a run and the
/// `diagnostic`s streamed ahead of it alike.
pub fn tagged_event<T: serde::Serialize>(kind: &str, value: &T) -> Result<String> {
    let mut doc = serde_json::json!({ "event": kind });
    let map = doc.as_object_mut().expect("an object");
    match serde_json::to_value(value)? {
        serde_json::Value::Object(fields) => map.extend(fields),
        other => {
            map.insert(kind.into(), other);
        }
    }
    Ok(serde_json::to_string(&doc)?)
}

pub use cambium_engine::hex;

/// Units every verb says a number in. They live in `cambium_ui` so the live
/// rows and the report on stdout cannot disagree about what `4.2 s` is.
pub use cambium_ui::report::human_bytes;

/// `one` after a count of one and `many` after any other, zero included:
/// `plural(n, "repository", "repositories")`.
pub fn plural<'a, N: PartialEq + From<u8>>(n: N, one: &'a str, many: &'a str) -> &'a str {
    if n == N::from(1) {
        one
    } else {
        many
    }
}

/// The `s` a regular noun takes after a count:
/// `format!("{n} file{}", plural_s(n))`.
pub fn plural_s<N: PartialEq + From<u8>>(n: N) -> &'static str {
    plural(n, "", "s")
}

/// The two halves of a two-dot range, read as `git diff` reads them: an
/// empty side is HEAD, so `main..` is `main..HEAD` and `..feature` is
/// `HEAD..feature`. `None` when there is no `..` at all, which each verb
/// reads its own way (a base for `affected`, the walk's end for `profile`).
/// A three-dot range is read before this by the verbs that take one
/// (`affected`, `explain`) and is a usage error for the others.
pub fn two_dot(range: &str) -> Option<(String, String)> {
    let (from, to) = range.split_once("..")?;
    let side = |s: &str| if s.is_empty() { "HEAD" } else { s }.to_string();
    Some((side(from), side(to)))
}

/// A span of seconds in the words someone would say it in: `3 days`,
/// `1 hour`. `status` and `--dry-run` say how old an index is this way;
/// [`human_age`] is the narration's shorter form.
pub fn duration(secs: u64) -> String {
    let (n, unit) = match secs {
        0..=119 => (secs, "second"),
        120..=3599 => (secs / 60, "minute"),
        3600..=86_399 => (secs / 3600, "hour"),
        _ => (secs / 86_400, "day"),
    };
    format!("{n} {unit}{}", plural_s(n))
}

/// An age in the coarsest unit that still says something, short enough for
/// a narration line: `40 min`, `4 h`, `2 d`. An index is minutes to weeks
/// old and nobody reads the seconds. [`duration`] is the spelled-out form.
pub fn human_age(secs: u64) -> String {
    match secs {
        0..=119 => format!("{secs} s"),
        120..=7_199 => format!("{} min", secs / 60),
        7_200..=172_799 => format!("{} h", secs / 3_600),
        _ => format!("{} d", secs / 86_400),
    }
}

/// A revision as the narration and the long reports name it: a sha cut to
/// ten digits, a ref name (`origin/main`) whole.
pub fn short(rev: &str) -> String {
    cambium_ui::report::rev(rev, 10)
}

/// A revision as the one-screen answers name it (`explain`, `--dry-run`): a
/// sha cut to the seven digits git abbreviates one to, a ref name whole.
pub fn git_short(rev: &str) -> String {
    cambium_ui::report::rev(rev, 7)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_is_singular_and_every_other_count_is_plural() {
        assert_eq!(plural(1usize, "repository", "repositories"), "repository");
        assert_eq!(plural(0u32, "repository", "repositories"), "repositories");
        assert_eq!(format!("{} file{}", 2u64, plural_s(2u64)), "2 files");
        assert_eq!(format!("{} file{}", 1u64, plural_s(1u64)), "1 file");
    }

    #[test]
    fn an_empty_side_of_a_range_is_head() {
        let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        assert_eq!(two_dot("a..b"), pair("a", "b"));
        assert_eq!(two_dot("a.."), pair("a", "HEAD"));
        assert_eq!(two_dot("..b"), pair("HEAD", "b"));
        assert_eq!(
            two_dot("origin/main~2..HEAD"),
            pair("origin/main~2", "HEAD")
        );
        assert_eq!(two_dot("main"), None);
    }

    #[test]
    fn an_age_is_singular_when_it_is_one() {
        assert_eq!(duration(3600), "1 hour");
        assert_eq!(duration(7200 * 2), "4 hours");
        assert_eq!(duration(86_400 * 3), "3 days");
    }

    /// A watcher reads the kind off `event` and the rest off the same
    /// object, so a document's fields sit beside the tag rather than under it.
    #[test]
    fn an_event_line_is_its_kind_beside_the_value_s_own_fields() {
        let read = |line: String| serde_json::from_str::<serde_json::Value>(&line).unwrap();
        let v = read(tagged_event("result", &serde_json::json!({ "schema": 18 })).unwrap());
        assert_eq!(v, serde_json::json!({ "event": "result", "schema": 18 }));
        let v = read(tagged_event("result", &"a bare value").unwrap());
        assert_eq!(
            v,
            serde_json::json!({ "event": "result", "result": "a bare value" })
        );
    }
}
