//! The old spellings, for one release.
//!
//! `docs/CLI.md` ("Migration") keeps every existing invocation working for a
//! release and has it say what to type instead. The renames are all shape
//! changes to the command line rather than to what a verb does, so they are
//! done here, on the argument vector, before clap sees it: the verbs
//! themselves carry no deprecated variants, the deprecated spellings are
//! therefore invisible in `--help` (which is what "hidden" has to mean), and
//! deleting this file after the release deletes the whole migration.
//!
//! Nothing is rendered here. The rewrite returns the diagnostics it raised and
//! `main` prints them once `--quiet` and `--format` have been parsed, so a
//! deprecation obeys the output contract like every other warning.

use cambium_engine::{Code, Diagnostic};

/// The queries that moved from `cambi index <q>` to `cambi inspect <q>`.
const QUERIES: &[&str] = &[
    "deps",
    "rdeps",
    "path",
    "consumers",
    "owner",
    "targets",
    "stats",
    "externals",
    "verify",
];

/// Global flags that take a separate value token, so that the scan for the
/// verb does not mistake `--log debug`'s value for it.
const VALUED: &[&str] = &[
    "--workspace",
    "-C",
    "--config",
    "--bazel-binary",
    "--cache-dir",
    "--log",
    "--format",
    "--output",
    "--color",
];

/// The command line as this `cambi` spells it, plus what had to be renamed.
pub struct Rewritten {
    pub argv: Vec<String>,
    pub notes: Vec<Diagnostic>,
}

fn deprecated(message: impl Into<String>) -> Diagnostic {
    Diagnostic::warning(Code::W001DeprecatedSpelling, message)
        .with_help("the old spelling is removed after one release")
}

/// Index of the first positional token in `argv` at or after `from`,
/// skipping flags and the values of the flags that take one. A `--` makes
/// the token after it positional whatever it looks like.
fn positional(argv: &[String], from: usize) -> Option<usize> {
    let mut i = from;
    while i < argv.len() {
        let tok = &argv[i];
        if tok == "--" {
            return argv.get(i + 1).map(|_| i + 1);
        }
        if tok.starts_with('-') {
            if VALUED.contains(&tok.as_str()) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        return Some(i);
    }
    None
}

/// Rewrites the old spellings into the new ones.
pub fn rewrite(mut argv: Vec<String>) -> Rewritten {
    let mut notes = Vec::new();
    flag_notes(&argv, &mut notes);

    let Some(verb) = positional(&argv, 1) else {
        return Rewritten { argv, notes };
    };
    let sub = positional(&argv, verb + 1).map(|i| (i, argv[i].clone()));

    match argv[verb].as_str() {
        // `baseline` is `index`, whatever follows it.
        "baseline" => {
            argv[verb] = "index".into();
            match &sub {
                Some((_, s)) => notes.push(deprecated(format!(
                    "cambi baseline {s} is now cambi index {s}"
                ))),
                None => notes.push(deprecated("cambi baseline is now cambi index")),
            }
        }
        // The queries under `index` are `inspect`; `index inspect` is either
        // `index show` or one of the two selectors, now positional verbs.
        "index" => match sub.as_ref().map(|(i, s)| (*i, s.as_str())) {
            Some((_, q)) if QUERIES.contains(&q) => {
                argv[verb] = "inspect".into();
                notes.push(deprecated(format!(
                    "cambi index {q} is now cambi inspect {q}"
                )));
            }
            Some((i, "inspect")) => rewrite_inspect(&mut argv, verb, i, &mut notes),
            _ => {}
        },
        // `--explain` on `affected` is the verb `cambi explain`. The flag
        // still works and still behaves exactly as it did; it is hidden from
        // help and says what to type instead.
        "affected" => {
            if argv.iter().any(|t| t == "--explain") {
                notes.push(deprecated("--explain on affected is now cambi explain"));
            }
        }
        // Bare `cambi config` printed the configuration; that is `config show`.
        "config" if sub.is_none() => {
            argv.push("show".into());
            notes.push(deprecated("cambi config is now cambi config show"));
        }
        _ => {}
    }

    Rewritten { argv, notes }
}

/// `index inspect` with a selector became a verb of its own; without one it
/// is `index show`.
fn rewrite_inspect(argv: &mut Vec<String>, verb: usize, sub: usize, notes: &mut Vec<Diagnostic>) {
    for (flag, new) in [("--label", "target"), ("--package", "package")] {
        let Some(value) = take_flag(argv, flag) else {
            continue;
        };
        argv[verb] = "inspect".into();
        argv[sub] = new.into();
        argv.push(value);
        notes.push(deprecated(format!(
            "cambi index inspect {flag} is now cambi inspect {new}"
        )));
        return;
    }
    argv[sub] = "show".into();
    notes.push(deprecated("cambi index inspect is now cambi index show"));
}

/// Removes `--flag value` or `--flag=value` from `argv` and returns the value.
fn take_flag(argv: &mut Vec<String>, flag: &str) -> Option<String> {
    let eq = format!("{flag}=");
    for i in 0..argv.len() {
        if let Some(v) = argv[i].strip_prefix(&eq) {
            let v = v.to_string();
            argv.remove(i);
            return Some(v);
        }
        if argv[i] == flag && i + 1 < argv.len() {
            let v = argv[i + 1].clone();
            argv.drain(i..=i + 1);
            return Some(v);
        }
    }
    None
}

/// `--output` and the `text` value keep working and say what they are now.
fn flag_notes(argv: &[String], notes: &mut Vec<Diagnostic>) {
    let mut said_output = false;
    let mut said_text = false;
    for (i, tok) in argv.iter().enumerate() {
        let (flag, value) = match tok.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (tok.as_str(), argv.get(i + 1).cloned()),
        };
        if flag == "--output" && !said_output {
            said_output = true;
            notes.push(deprecated("--output is now --format"));
        }
        if matches!(flag, "--output" | "--format") && value.as_deref() == Some("text") && !said_text
        {
            said_text = true;
            notes.push(deprecated("--format text is now --format human"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn go(args: &[&str]) -> (Vec<String>, Vec<String>) {
        let r = rewrite(
            std::iter::once("cambi")
                .chain(args.iter().copied())
                .map(String::from)
                .collect(),
        );
        (
            r.argv[1..].to_vec(),
            r.notes.iter().map(|d| d.message.clone()).collect(),
        )
    }

    #[test]
    fn baseline_is_index() {
        let (argv, notes) = go(&["baseline", "build", "--publish"]);
        assert_eq!(argv, ["index", "build", "--publish"]);
        assert_eq!(notes, ["cambi baseline build is now cambi index build"]);
    }

    #[test]
    fn the_queries_are_inspect() {
        let (argv, notes) = go(&["index", "stats", "x.cmbx"]);
        assert_eq!(argv, ["inspect", "stats", "x.cmbx"]);
        assert_eq!(notes, ["cambi index stats is now cambi inspect stats"]);
        // The store verbs under `index` are not renamed and say nothing.
        let (argv, notes) = go(&["index", "list"]);
        assert_eq!(argv, ["index", "list"]);
        assert!(notes.is_empty());
    }

    #[test]
    fn the_selectors_became_verbs() {
        let (argv, _) = go(&["index", "inspect", "x.cmbx", "--label", "//a:b"]);
        assert_eq!(argv, ["inspect", "target", "x.cmbx", "//a:b"]);
        let (argv, _) = go(&["index", "inspect", "x.cmbx", "--package=//a"]);
        assert_eq!(argv, ["inspect", "package", "x.cmbx", "//a"]);
        let (argv, notes) = go(&["index", "inspect", "x.cmbx"]);
        assert_eq!(argv, ["index", "show", "x.cmbx"]);
        assert_eq!(notes, ["cambi index inspect is now cambi index show"]);
    }

    #[test]
    fn a_global_flags_value_is_not_read_as_the_verb() {
        let (argv, notes) = go(&["--log", "debug", "baseline", "list"]);
        assert_eq!(argv, ["--log", "debug", "index", "list"]);
        assert_eq!(notes, ["cambi baseline list is now cambi index list"]);
    }

    #[test]
    fn the_explain_flag_on_affected_names_the_verb_that_replaced_it() {
        let (argv, notes) = go(&["affected", "a..b", "--explain"]);
        assert_eq!(
            argv,
            ["affected", "a..b", "--explain"],
            "the flag still works; only the advice is new"
        );
        assert_eq!(notes, ["--explain on affected is now cambi explain"]);
        let (_, notes) = go(&["affected", "a..b"]);
        assert!(notes.is_empty());
        // The verb of the same name is not a deprecated spelling of itself.
        let (_, notes) = go(&["explain", "//a:b"]);
        assert!(notes.is_empty());
    }

    #[test]
    fn bare_config_is_config_show() {
        let (argv, notes) = go(&["config"]);
        assert_eq!(argv, ["config", "show"]);
        assert_eq!(notes, ["cambi config is now cambi config show"]);
        let (argv, notes) = go(&["config", "hash"]);
        assert_eq!(argv, ["config", "hash"]);
        assert!(notes.is_empty());
    }

    #[test]
    fn the_old_output_flag_and_value_are_named_once_each() {
        let (_, notes) = go(&["index", "list", "--output", "text"]);
        assert_eq!(
            notes,
            [
                "--output is now --format",
                "--format text is now --format human"
            ]
        );
        let (_, notes) = go(&["index", "list", "--format=json"]);
        assert!(notes.is_empty());
    }
}
