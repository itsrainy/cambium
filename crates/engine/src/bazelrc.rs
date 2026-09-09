//! Reading a `.bazelrc` diff instead of surrendering to it.
//!
//! `.bazelrc` is a default invalidator, and for a good reason: a line in it can
//! change what `bazel query` sees — `--define`, `--platforms`, a starlark flag,
//! an `import` of another file, a `--config` every later command names. Any of
//! those moves the graph, and nothing in the index records which flags produced
//! it, so the only sound answer is "everything changed".
//!
//! But most `.bazelrc` edits are not that. Across the real subjects, the commits whose
//! *only* global reason is `.bazelrc` are overwhelmingly remote-cache endpoints,
//! `--jobs`, test-output verbosity and BES bookkeeping: flags that select how
//! the build is executed and reported, never what is loaded or analysed. This
//! module parses both sides of the diff into (command, config, flag) triples,
//! and lets a run past only when *every* changed triple is on an allowlist of
//! flags known not to reach loading or analysis for any command. Everything
//! else — an unknown flag, an `import`, a startup option, a non-UTF-8 file, the
//! file appearing or disappearing — is still global.
//!
//! The allowlist is the whole safety argument, so it is written as data with a
//! reason per entry, and it errs towards "unknown": a flag that is merely
//! *probably* irrelevant costs one over-invalidation if it is left off, and a
//! missed target if it is wrongly put on.
//!
//! # Starlark flags
//!
//! One family off the allowlist can still be answered exactly, because it is
//! not a flag at all but a *target*: a Starlark build setting, written
//! `--@envoy//bazel:crypto=boringssl` or `--//bazel:foo=bar`. [`screen`] hands
//! those back to the engine instead of calling them unknown; the engine
//! resolves each against the index and either seeds its node or keeps the
//! global. The reasoning it relies on:
//!
//! - A `config_setting(flag_values = {"//bazel:crypto": "boringssl"})` names
//!   the flag through a label-keyed attribute, so `bazel query` records an edge
//!   from the config_setting to the flag, and every `select()` key is an edge
//!   from the selecting rule to the config_setting. Seeding the flag's node
//!   into the direct set and running the ordinary reverse walk therefore
//!   reaches every target whose definition the value can move, exactly as the
//!   externals stage does for a moved repository's leaf nodes. The same goes
//!   for a rule reading the setting through `attr.label(default = ...)`: the
//!   default is an edge.
//! - A Starlark *transition* reading the flag (`transition(inputs = [...])`) is
//!   not an edge. Its `inputs`/`outputs` are string literals in a `.bzl`, so
//!   the engine looks for the label in the text of every `.bzl` in the stored
//!   load closure; a hit means the walk cannot be trusted and the run stays
//!   global, naming the file.
//! - A setting in an external repository keeps its global: the index does not
//!   cover that repository's graph, so what inside it reads the setting cannot
//!   be seen. Only the main repository's own settings are resolved.
//! - `command:config` scoping keeps its global too: a value that applies only
//!   under `--config=foo` is a question about the invocation, and cambium does
//!   not model invocations.
//!
//! `--define` and native flags are not labels and are untouched by any of this.

use std::collections::{BTreeMap, BTreeSet};

/// Whether a repository path is a bazelrc, and so gets read rather than merely
/// matched against the invalidator list.
///
/// Bazel reads `.bazelrc` from the workspace root, but repositories routinely
/// keep more of them (`user.bazelrc`, `ci.bazelrc`, `tools/bazel.rc`) and
/// `import` between them. The parser is the same for all of them; only the
/// import graph differs, and any import change is global anyway.
pub fn is_bazelrc_path(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.ends_with(".bazelrc") || base == "bazel.rc"
}

/// A changed flag whose name is a label: a Starlark build setting
/// (`--@envoy//bazel:crypto=boringssl`, `--//bazel:foo=bar`).
///
/// Unlike `--define` or a native flag, this one *is* a target, and the index
/// records every edge into it: `config_setting(flag_values = ...)` names it,
/// and a rule's `select()` names the config_setting. So the engine can answer
/// what a change to it reaches instead of giving up — see
/// [`crate::engine::Engine::compute`] and the module doc's "Starlark flags".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelFlag {
    pub key: FlagKey,
    /// The label as written in the rc file, dashes and `no` prefix removed.
    pub label: String,
}

/// The result of reading a `.bazelrc` diff: how many triples changed, which of
/// them were label-valued Starlark flags (resolvable against the index), and
/// the first thing that was neither allowlisted nor resolvable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Screen {
    pub changed_flags: usize,
    pub label_flags: Vec<LabelFlag>,
    pub unknown: Option<String>,
}

/// What a `.bazelrc` diff means for the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every changed flag is on the loading-irrelevant allowlist. The number of
    /// changed triples is carried for the stats line.
    Safe { changed_flags: usize },
    /// Something changed that could move the graph; the string says what.
    Unknown(String),
}

/// One allowlist entry: a flag-name pattern (`*` only as a trailing wildcard)
/// and why it cannot affect loading or analysis.
struct Allowed {
    pattern: &'static str,
    _why: &'static str,
}

const fn a(pattern: &'static str, why: &'static str) -> Allowed {
    Allowed { pattern, _why: why }
}

/// Flags whose value cannot change which packages load, which targets exist, or
/// what any target's configured definition is.
///
/// Deliberately *not* here, with the reason:
///
/// - `--repository_cache`, `--registry`, `--override_repository`,
///   `--override_module`: they change which external code is fetched, and
///   external code defines rules.
/// - `--workspace_status_command`: stamping is invisible to the graph today,
///   but the command's output reaches actions, and the margin is not worth it.
/// - `--output_user_root` and every other startup option: startup options
///   restart the server and can carry `--bazelrc`/`--nosystem_rc`, which change
///   which files are read at all. Startup lines are never classified safe.
/// - `--define`, `--@repo//flag=`, `--//flag=`, `--platforms`, `--cpu`,
///   `--compilation_mode`, `--config`, `--enable_bzlmod`, `--deleted_packages`,
///   `--package_path`: all of these are the graph. They are unknown by falling
///   off this list; there is no separate deny list to keep in sync.
const ALLOWLIST: &[Allowed] = &[
    a(
        "remote_*",
        "remote cache/executor endpoints, headers, timeouts and retries: execution transport only",
    ),
    a("disk_cache", "where action outputs are cached locally"),
    a("jobs", "how many actions run at once"),
    a("local_resources", "scheduling budget for local actions"),
    a("local_cpu_resources", "scheduling budget for local actions"),
    a("local_ram_resources", "scheduling budget for local actions"),
    a("local_test_jobs", "how many tests run at once"),
    a("test_output", "how test output is printed"),
    a("test_summary", "how the test summary is printed"),
    a("test_timeout", "how long a test may run"),
    a(
        "test_verbose_timeout_warnings",
        "whether a timeout warning is printed",
    ),
    a("flaky_test_attempts", "how often a failed test is retried"),
    a(
        "cache_test_results",
        "whether cached test results are reused",
    ),
    a("runs_per_test", "how often a test is run"),
    a("show_progress*", "progress display"),
    a("show_loading_progress", "progress display"),
    a("show_timestamps", "progress display"),
    a("show_result", "how many results are printed at the end"),
    a("progress_*", "progress display"),
    a("color", "terminal colour"),
    a("curses", "terminal cursor control"),
    a(
        "verbose_failures",
        "whether failing command lines are printed",
    ),
    a("announce_rc", "whether the rc lines in effect are printed"),
    a("bes_*", "build event service upload"),
    a("build_event_*", "build event stream output"),
    a("experimental_ui_*", "terminal UI"),
    a("ui_*", "terminal UI"),
    a(
        "keep_going",
        "whether the build stops at the first failure; the graph is the same either way",
    ),
    a(
        "sandbox_*",
        "how actions are isolated when they run; analysis has already happened",
    ),
    a("spawn_strategy", "which executor runs an action"),
    a("strategy", "which executor runs an action"),
    a("genrule_strategy", "which executor runs a genrule"),
    a("worker_*", "persistent worker pool behaviour"),
    a("profile", "where the JSON profile is written"),
    a("memory_profile", "where the memory profile is written"),
    a(
        "build_metadata",
        "key/value metadata attached to the build event stream",
    ),
    a("max_idle_secs", "how long an idle server lives"),
    a("io_nice_level", "process scheduling priority"),
    a(
        "watchfs",
        "whether the server watches the filesystem for changes",
    ),
];

fn pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

/// Whether a flag name (no leading dashes) is loading-irrelevant.
///
/// A boolean flag can be written `--foo` or `--nofoo`; the negation is the same
/// flag, so `nofoo` is retried as `foo` when the literal name misses.
fn is_allowed(name: &str, extra: &[String]) -> bool {
    let hit = |n: &str| {
        ALLOWLIST.iter().any(|e| pattern_matches(e.pattern, n))
            || extra
                .iter()
                .any(|p| pattern_matches(p.trim_start_matches('-'), n))
    };
    hit(name) || name.strip_prefix("no").is_some_and(hit)
}

/// A flag as it appears in one rc file: which command it applies to, under
/// which `--config` name, and its own name with the dashes stripped.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FlagKey {
    pub command: String,
    pub config: Option<String>,
    pub name: String,
}

/// A parsed bazelrc: flag values keyed by triple, plus the files it imports.
///
/// Values are sorted rather than kept in order, so that moving a line without
/// changing what it says is not a change. Repeats are kept, because
/// `--copt=-O2 --copt=-O2` is not `--copt=-O2` to Bazel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bazelrc {
    pub flags: BTreeMap<FlagKey, Vec<String>>,
    /// `import` and `try-import` targets, with the keyword, in file order.
    pub imports: BTreeSet<String>,
}

/// Parses a bazelrc's text.
///
/// Comments (`#` to end of line), blank lines, backslash line continuations,
/// `command:config` prefixes, `import`/`try-import`, and single- or
/// double-quoted tokens. Nothing here fails: a line the parser does not
/// understand becomes a flag with an unrecognisable name, which is unknown, and
/// unknown is global.
pub fn parse(text: &str) -> Bazelrc {
    let mut rc = Bazelrc::default();
    for line in logical_lines(text) {
        let tokens = tokenize(&line);
        let Some((head, rest)) = tokens.split_first() else {
            continue;
        };
        if head == "import" || head == "try-import" {
            for t in rest {
                rc.imports.insert(format!("{head} {t}"));
            }
            continue;
        }
        let (command, config) = match head.split_once(':') {
            Some((c, cfg)) => (c.to_string(), Some(cfg.to_string())),
            None => (head.clone(), None),
        };
        let mut i = 0;
        while i < rest.len() {
            let tok = &rest[i];
            i += 1;
            let (name, value) = match tok.split_once('=') {
                Some((n, v)) => (n.to_string(), v.to_string()),
                None => {
                    // `--jobs 8`: a following token that is not itself a flag
                    // is this flag's value. A positional that follows nothing
                    // keeps itself as its value, so it still counts as changed
                    // when it moves.
                    let v = match rest.get(i) {
                        Some(next) if !next.starts_with('-') && tok.starts_with('-') => {
                            i += 1;
                            next.clone()
                        }
                        _ => String::new(),
                    };
                    (tok.clone(), v)
                }
            };
            let key = FlagKey {
                command: command.clone(),
                config: config.clone(),
                name: name.trim_start_matches('-').to_string(),
            };
            rc.flags.entry(key).or_default().push(value);
        }
    }
    for values in rc.flags.values_mut() {
        values.sort();
    }
    rc
}

/// Joins backslash continuations and strips comments, yielding one string per
/// logical line.
fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = String::new();
    for raw in text.lines() {
        let line = strip_comment(raw);
        match line.strip_suffix('\\') {
            Some(head) => {
                pending.push_str(head);
                pending.push(' ');
            }
            None => {
                pending.push_str(&line);
                if !pending.trim().is_empty() {
                    out.push(std::mem::take(&mut pending));
                } else {
                    pending.clear();
                }
            }
        }
    }
    if !pending.trim().is_empty() {
        out.push(pending);
    }
    out
}

/// Everything before an unquoted `#` that starts a token.
fn strip_comment(line: &str) -> String {
    let mut quote: Option<char> = None;
    let mut prev_ws = true;
    for (i, ch) in line.char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '#' && prev_ws => return line[..i].trim_end().to_string(),
            None => {}
        }
        prev_ws = ch.is_whitespace();
    }
    line.trim_end().to_string()
}

/// Whitespace-separated tokens, honouring single and double quotes.
fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for ch in line.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                started = true;
            }
            None if ch.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => {
                cur.push(ch);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// The triples whose values differ between two parsed files, in a stable order.
pub fn changed_flags(base: &Bazelrc, head: &Bazelrc) -> Vec<FlagKey> {
    let mut keys: BTreeSet<&FlagKey> = base.flags.keys().collect();
    keys.extend(head.flags.keys());
    keys.into_iter()
        .filter(|k| base.flags.get(*k) != head.flags.get(*k))
        .cloned()
        .collect()
}

/// The label a flag name denotes, if it is one.
///
/// `@repo//pkg:name`, `@@canonical//pkg:name` and `//pkg:name` are Starlark
/// build settings; everything else (`define`, `jobs`, `platforms`) is a native
/// flag and has no target behind it. A boolean setting is negated by writing
/// `--no//pkg:name`, so a leading `no` in front of a label is stripped — and
/// only in front of a label, so `nostamp` stays `nostamp`.
///
/// `name` has already had its leading dashes removed by [`parse`].
pub fn flag_label(name: &str) -> Option<&str> {
    let label = match name.strip_prefix("no") {
        Some(rest) if rest.starts_with('@') || rest.starts_with("//") => rest,
        _ => name,
    };
    let rest = if let Some(r) = label.strip_prefix("@@") {
        r
    } else if let Some(r) = label.strip_prefix('@') {
        r
    } else {
        return label.starts_with("//").then_some(label);
    };
    // `@repo//pkg:name`: a repository name and then a main-repo-shaped label.
    let (repo, after) = rest.split_once("//")?;
    (!repo.is_empty() && !repo.contains(':') && !after.is_empty()).then_some(label)
}

/// Reads a bazelrc diff into the three things a run can act on: the count of
/// changed triples, the label-valued Starlark flags among them, and the first
/// change that is neither allowlisted nor a label.
///
/// A label flag is only handed back when it is set unconditionally: a value
/// written under `command:config` applies only when that `--config` is named,
/// and cambium does not model configs, so it cannot tell whether the run it is
/// answering for uses it. Startup lines and imports stay unknown.
pub fn screen(base_text: &str, head_text: &str, extra_allowlist: &[String]) -> Screen {
    let base = parse(base_text);
    let head = parse(head_text);
    let mut out = Screen::default();
    if base.imports != head.imports {
        let mut diff: Vec<&String> = base.imports.symmetric_difference(&head.imports).collect();
        diff.sort();
        let first = diff.first().map(|s| s.as_str()).unwrap_or("import");
        out.unknown = Some(format!(
            "`{first}` changed; an imported rc file can carry anything"
        ));
        return out;
    }
    let changed = changed_flags(&base, &head);
    out.changed_flags = changed.len();
    for key in &changed {
        if key.command == "startup" {
            out.unknown = Some(format!(
                "startup --{} changed, and startup options are never classified",
                key.name
            ));
            return out;
        }
        if is_allowed(&key.name, extra_allowlist) {
            continue;
        }
        match flag_label(&key.name) {
            Some(label) if key.config.is_none() => out.label_flags.push(LabelFlag {
                key: key.clone(),
                label: label.to_string(),
            }),
            Some(_) => {
                out.unknown = Some(format!(
                    "{}:{} --{} changed, and a flag set under a --config applies only to \
                     the runs that name it, which cambium does not model",
                    key.command,
                    key.config.as_deref().unwrap_or_default(),
                    key.name
                ));
                return out;
            }
            None => {
                out.unknown = Some(format!(
                    "{} --{} changed, not on the loading-irrelevant allowlist",
                    key.command, key.name
                ));
                return out;
            }
        }
    }
    out
}

/// Classifies a bazelrc edit without an index to resolve label flags against:
/// safe only if every changed triple is allowlisted and the imports are
/// untouched. A Starlark flag is unknown here; [`screen`] is what the engine
/// uses, and it can do better.
pub fn classify(base_text: &str, head_text: &str, extra_allowlist: &[String]) -> Verdict {
    let s = screen(base_text, head_text, extra_allowlist);
    if let Some(why) = s.unknown {
        return Verdict::Unknown(why);
    }
    if let Some(f) = s.label_flags.first() {
        return Verdict::Unknown(format!(
            "{} --{} changed, not on the loading-irrelevant allowlist",
            f.key.command, f.key.name
        ));
    }
    Verdict::Safe {
        changed_flags: s.changed_flags,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(command: &str, config: Option<&str>, name: &str) -> FlagKey {
        FlagKey {
            command: command.into(),
            config: config.map(String::from),
            name: name.into(),
        }
    }

    #[test]
    fn parses_commands_configs_and_comments() {
        let rc = parse(
            "# a comment\n\
             build --jobs=8   # trailing comment\n\
             build:ci --remote_cache=grpc://x\n\
             \n\
             test --test_output=errors\n",
        );
        assert_eq!(rc.flags[&key("build", None, "jobs")], vec!["8"]);
        assert_eq!(
            rc.flags[&key("build", Some("ci"), "remote_cache")],
            vec!["grpc://x"]
        );
        assert_eq!(rc.flags[&key("test", None, "test_output")], vec!["errors"]);
        assert!(rc.imports.is_empty());
    }

    #[test]
    fn joins_line_continuations() {
        let rc = parse("build --jobs=8 \\\n  --remote_cache=x \\\n  --color=yes\n");
        assert_eq!(rc.flags[&key("build", None, "color")], vec!["yes"]);
        assert_eq!(rc.flags[&key("build", None, "remote_cache")], vec!["x"]);
    }

    #[test]
    fn separate_value_tokens_and_quotes() {
        let rc = parse("build --jobs 8 --build_metadata=\"a b\"\n");
        assert_eq!(rc.flags[&key("build", None, "jobs")], vec!["8"]);
        assert_eq!(rc.flags[&key("build", None, "build_metadata")], vec!["a b"]);
    }

    #[test]
    fn imports_are_recorded() {
        let rc = parse("import %workspace%/other.bazelrc\ntry-import %workspace%/user.bazelrc\n");
        assert_eq!(rc.imports.len(), 2);
        assert!(rc.imports.iter().any(|i| i.starts_with("try-import ")));
    }

    #[test]
    fn reordering_and_whitespace_are_not_changes() {
        let a = "build --jobs=8\nbuild --color=yes\n";
        let b = "\n\nbuild   --color=yes\n# note\nbuild --jobs=8\n";
        assert!(changed_flags(&parse(a), &parse(b)).is_empty());
        assert_eq!(classify(a, b, &[]), Verdict::Safe { changed_flags: 0 });
    }

    #[test]
    fn added_removed_and_changed_values_are_all_changes() {
        let base = "build --jobs=8\nbuild --color=yes\n";
        let head = "build --jobs=16\nbuild --test_output=errors\n";
        let changed = changed_flags(&parse(base), &parse(head));
        let names: Vec<&str> = changed.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, vec!["color", "jobs", "test_output"]);
    }

    #[test]
    fn a_config_is_part_of_the_key() {
        let changed = changed_flags(
            &parse("build --jobs=8\n"),
            &parse("build --jobs=8\nbuild:ci --jobs=8\n"),
        );
        assert_eq!(changed, vec![key("build", Some("ci"), "jobs")]);
    }

    #[test]
    fn allowlisted_edits_are_safe() {
        let base = "build --remote_cache=grpc://old\nbuild --jobs=8\ntest --test_output=summary\n";
        let head = "build --remote_cache=grpc://new\nbuild --jobs=16\ntest --test_output=errors\n\
                    build --bes_backend=grpc://bes\nbuild --noshow_progress\n";
        assert_eq!(
            classify(base, head, &[]),
            Verdict::Safe { changed_flags: 5 }
        );
    }

    #[test]
    fn a_define_is_unknown() {
        let v = classify(
            "build --jobs=8\n",
            "build --jobs=8\nbuild --define=x=1\n",
            &[],
        );
        assert_eq!(
            v,
            Verdict::Unknown(
                "build --define changed, not on the loading-irrelevant allowlist".into()
            )
        );
    }

    #[test]
    fn a_starlark_flag_and_a_config_reference_are_unknown() {
        for line in [
            "build --//flags:mode=fast",
            "build --@rules_go//go/config:race",
            "build --config=ci",
            "build --platforms=//p:linux",
            "build --enable_bzlmod",
            "build --override_repository=foo=/tmp/foo",
        ] {
            let v = classify(
                "build --jobs=8\n",
                &format!("build --jobs=8\n{line}\n"),
                &[],
            );
            assert!(matches!(v, Verdict::Unknown(_)), "{line} classified {v:?}");
        }
    }

    #[test]
    fn a_startup_line_is_unknown_even_when_the_flag_looks_harmless() {
        let v = classify(
            "startup --max_idle_secs=10\n",
            "startup --max_idle_secs=20\n",
            &[],
        );
        assert!(
            matches!(&v, Verdict::Unknown(m) if m.contains("startup")),
            "{v:?}"
        );
    }

    #[test]
    fn an_added_import_is_unknown() {
        let v = classify(
            "build --jobs=8\n",
            "build --jobs=8\nimport %workspace%/ci.bazelrc\n",
            &[],
        );
        assert!(
            matches!(&v, Verdict::Unknown(m) if m.contains("import")),
            "{v:?}"
        );
    }

    #[test]
    fn the_extra_allowlist_extends_it() {
        let base = "build --my_local_flag=1\n";
        let head = "build --my_local_flag=2\n";
        assert!(matches!(classify(base, head, &[]), Verdict::Unknown(_)));
        assert_eq!(
            classify(base, head, &["--my_local_*".to_string()]),
            Verdict::Safe { changed_flags: 1 }
        );
    }

    #[test]
    fn negated_booleans_match_their_positive_form() {
        assert!(is_allowed("nocache_test_results", &[]));
        assert!(!is_allowed("nodefine", &[]));
    }

    #[test]
    fn label_flags_are_recognised_in_every_spelling() {
        assert_eq!(flag_label("@repo//x:y"), Some("@repo//x:y"));
        assert_eq!(flag_label("@@repo+//x:y"), Some("@@repo+//x:y"));
        assert_eq!(flag_label("//x:y"), Some("//x:y"));
        assert_eq!(flag_label("//x/y:z"), Some("//x/y:z"));
        // A package with no target part is still a label.
        assert_eq!(flag_label("@quiche//:ssl_lib"), Some("@quiche//:ssl_lib"));
        // Boolean negation of a setting, and a native flag that starts `no`.
        assert_eq!(flag_label("no//x:y"), Some("//x:y"));
        assert_eq!(flag_label("no@repo//x:y"), Some("@repo//x:y"));
        assert_eq!(flag_label("nostamp"), None);
        assert_eq!(flag_label("nocache_test_results"), None);
        for native in ["define", "jobs", "platforms", "config", "", "@", "@repo"] {
            assert_eq!(flag_label(native), None, "{native}");
        }
    }

    #[test]
    fn a_label_flag_is_handed_back_rather_than_called_unknown() {
        let s = screen(
            "build --jobs=8\n",
            "build --jobs=8\nbuild --@envoy//bazel:crypto=boringssl\nbuild --//bazel:foo=bar\n",
            &[],
        );
        assert_eq!(s.unknown, None);
        assert_eq!(s.changed_flags, 2);
        let labels: Vec<&str> = s.label_flags.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, ["//bazel:foo", "@envoy//bazel:crypto"]);
    }

    #[test]
    fn a_label_flag_under_a_config_is_unknown() {
        let s = screen(
            "build --jobs=8\n",
            "build --jobs=8\nbuild:ci --@envoy//bazel:crypto=boringssl\n",
            &[],
        );
        assert!(s.label_flags.is_empty());
        assert!(
            s.unknown.as_deref().is_some_and(|m| m.contains("--config")),
            "{:?}",
            s.unknown
        );
    }

    #[test]
    fn an_unknown_native_flag_still_wins_over_a_label_flag() {
        let s = screen(
            "build --jobs=8\n",
            "build --jobs=8\nbuild --//bazel:foo=bar\nbuild --define=x=1\n",
            &[],
        );
        assert!(s.unknown.is_some(), "{s:?}");
    }

    #[test]
    fn bazelrc_paths() {
        for p in [
            ".bazelrc",
            "user.bazelrc",
            "tools/ci.bazelrc",
            "tools/bazel.rc",
        ] {
            assert!(is_bazelrc_path(p), "{p}");
        }
        for p in ["BUILD", "src/.bazelversion", "foo/bar.bzl"] {
            assert!(!is_bazelrc_path(p), "{p}");
        }
    }
}
