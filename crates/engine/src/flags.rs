//! Resolving a changed Starlark build setting against the index.
//!
//! [`crate::bazelrc`] decides that a `.bazelrc` edit changed a label-valued
//! flag and hands the labels here; this module decides what each one reaches.
//! The claim, in full:
//!
//! 1. **Edges.** A `config_setting` names the flag in `flag_values`, which is a
//!    label-keyed attribute, so `bazel query` emits an edge from the
//!    config_setting to the flag target; a rule's `select()` keys are edges to
//!    the config_settings; a rule reading the setting through
//!    `attr.label(default = "//bazel:crypto")` has that default as an edge.
//!    Every way a *target's* definition can depend on the setting's value is
//!    therefore an edge in the stored graph, and the ordinary reverse walk from
//!    the flag's node covers all of them. This is the externals stage's
//!    argument (`crate::externals::reach`) applied to one node instead of a
//!    repository's worth.
//! 2. **Transitions.** A Starlark transition declaring the flag in `inputs` or
//!    `outputs` changes the configuration of targets that no edge connects to
//!    it. `inputs`/`outputs` are string literals in a `.bzl`, so the text of
//!    every `.bzl` in the stored load closure is scanned for the label; a hit
//!    means the answer cannot be trusted and the run stays global. So does a
//!    closure that cannot be read at all, or an index built without one.
//! 3. **External settings.** A setting that lives in an external repository —
//!    `@quiche//:ssl_lib`, `@protobuf//bazel/flags:prefer_prebuilt_protoc` —
//!    is resolved by nobody: the index covers the main repository only, so the
//!    `config_setting`s, `select()`s and transitions *inside* that repository
//!    are not edges cambium holds, and the main-repo targets that depend on
//!    them through ordinary rule targets would be missed. The edges into a
//!    main-repo rule that names the setting directly are the only part of the
//!    picture the index has, and a part is not an answer. Those stay global.
//!    (The main repository's own name is not external: `@envoy//bazel:crypto`
//!    inside envoy is `//bazel:crypto`, and the repository mapping says so.)
//! 4. **No node.** If a main-repo label resolves to no node in the index,
//!    nothing depends on the setting through an edge; with (2) clean, the edit
//!    affects nothing at all.
//!
//! A flag's *value* changing does not change any rule's definition, so nothing
//! is re-queried: the resolved nodes join the direct-set seeds, like external
//! nodes do, and the walk does the rest.

use cambium_index::IndexRef;

use crate::bazelrc::LabelFlag;
use crate::query::PackageQuerier;
use crate::reason::{FlagCause, GlobalReason};

/// What a set of changed Starlark build settings reaches.
#[derive(Default, Debug)]
pub struct FlagReach {
    /// Flag nodes to seed the direct set with, as `(node id, label)`.
    pub nodes: Vec<(u32, String)>,
    /// Every flag that was resolved this way, as written in the rc file.
    pub flags: Vec<String>,
    /// Set when one of them could not be resolved soundly; the run is global.
    pub global: Option<GlobalReason>,
}

/// A label split into the parts the index is searched by.
struct Parsed<'a> {
    /// The apparent (or canonical) repository name, empty for the main repo.
    repo: &'a str,
    /// `pkg:name`, as an index label writes it after the `//`.
    suffix: String,
}

/// `@repo//pkg:name`, `@@canonical//pkg:name`, `//pkg:name`, and the shorthand
/// `//pkg` whose target name is the package's last segment.
fn parse_label(label: &str) -> Option<Parsed<'_>> {
    let (repo, rest) = match label.strip_prefix("@@") {
        Some(rest) => rest.split_once("//")?,
        None => match label.strip_prefix('@') {
            Some(rest) => rest.split_once("//")?,
            None => ("", label.strip_prefix("//")?),
        },
    };
    let suffix = match rest.split_once(':') {
        Some(_) => rest.to_string(),
        // `--@repo//pkg` is `@repo//pkg:pkg`.
        None => format!("{rest}:{}", rest.rsplit('/').next().unwrap_or(rest)),
    };
    Some(Parsed { repo, suffix })
}

/// Whether any `.bzl` in the stored closure names this label as a string
/// literal — a transition's `inputs`/`outputs`, or anything else that reads the
/// setting without an edge. Returns the path of the first file that does.
///
/// The search is for the `//pkg:name` part alone, so it matches every spelling
/// a `.bzl` can write (`//bazel:crypto`, `@envoy//bazel:crypto`,
/// `@@envoy+//bazel:crypto`) at the cost of also matching a same-named setting
/// in another repository. That direction is the safe one. A trailing label
/// character is not a match, so `//bazel:crypto` does not fire on
/// `//bazel:crypto_lib`.
fn bzl_witness(
    base: IndexRef<'_>,
    src: &dyn cambium_index::FileText,
    suffix: &str,
) -> Option<String> {
    let needle = format!("//{suffix}");
    for id in 0..base.num_bzl() {
        let path = base.bzl_path(id);
        let Some(text) = src.text(path) else {
            continue;
        };
        if mentions(&text, &needle) {
            return Some(path.to_string());
        }
    }
    None
}

/// `needle` occurs in `text` and is not the prefix of a longer target name.
fn mentions(text: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(i) = text[from..].find(needle) {
        let end = from + i + needle.len();
        let next = text[end..].chars().next();
        if !next.is_some_and(|c| c.is_alphanumeric() || "_.+-".contains(c)) {
            return true;
        }
        from = end;
    }
    false
}

/// Resolves every changed Starlark build setting, or says why the run must stay
/// global.
pub fn resolve(base: IndexRef<'_>, querier: &dyn PackageQuerier, flags: &[LabelFlag]) -> FlagReach {
    let mut out = FlagReach::default();
    if flags.is_empty() {
        return out;
    }
    let Some(src) = querier.load_source() else {
        out.global = Some(reason(&flags[0], FlagCause::NoStarlarkSource));
        return out;
    };
    if !base.bzl_closure {
        out.global = Some(reason(&flags[0], FlagCause::NoLoadClosure));
        return out;
    }
    let facts = cambium_index::read_module_facts(src);
    let mapping = querier
        .repo_mapping()
        .with_main_alias(&facts.module_name)
        .with_main_alias("__main__");

    for f in flags {
        let Some(p) = parse_label(&f.label) else {
            out.global = Some(reason(f, FlagCause::Unparsed));
            return out;
        };
        let main = mapping.is_main(p.repo);
        if let Some(path) = bzl_witness(base, src, &p.suffix) {
            out.global = Some(reason(f, FlagCause::ReadByBzl { path }));
            return out;
        }
        if !main {
            out.global = Some(reason(
                f,
                FlagCause::External {
                    repo: p.repo.to_string(),
                },
            ));
            return out;
        }
        out.flags.push(f.label.clone());
        match base.id_of(&format!("//{}", p.suffix)) {
            Some(id) => {
                tracing::debug!(
                    flag = %f.label,
                    label = %base.label(id),
                    "starlark build setting resolved"
                );
                out.nodes.push((id, base.label(id).to_string()));
            }
            // Nothing names it, and no `.bzl` reads it: the edit reaches
            // nothing at all.
            None => tracing::debug!(
                flag = %f.label,
                "starlark build setting is in no rule's dependencies; the edit affects nothing"
            ),
        }
    }
    out.nodes.sort_unstable();
    out.nodes.dedup();
    out
}

fn reason(f: &LabelFlag, cause: FlagCause) -> GlobalReason {
    GlobalReason::Flag {
        command: f.key.command.clone(),
        name: f.key.name.clone(),
        cause,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_split_into_repository_and_suffix() {
        let p = parse_label("@envoy//bazel:crypto").unwrap();
        assert_eq!((p.repo, p.suffix.as_str()), ("envoy", "bazel:crypto"));
        let p = parse_label("@@envoy+//bazel:crypto").unwrap();
        assert_eq!((p.repo, p.suffix.as_str()), ("envoy+", "bazel:crypto"));
        let p = parse_label("//bazel:crypto").unwrap();
        assert_eq!((p.repo, p.suffix.as_str()), ("", "bazel:crypto"));
        let p = parse_label("@quiche//:ssl_lib").unwrap();
        assert_eq!((p.repo, p.suffix.as_str()), ("quiche", ":ssl_lib"));
        let p = parse_label("//bazel/foreign_cc").unwrap();
        assert_eq!(p.suffix, "bazel/foreign_cc:foreign_cc");
        assert!(parse_label("define").is_none());
    }

    #[test]
    fn a_mention_is_the_whole_target_name() {
        assert!(mentions(
            "transition(inputs = [\"//bazel:crypto\"])",
            "//bazel:crypto"
        ));
        assert!(mentions("x = \"@envoy//bazel:crypto\"\n", "//bazel:crypto"));
        assert!(!mentions(
            "deps = [\"//bazel:crypto_lib\"]",
            "//bazel:crypto"
        ));
        assert!(!mentions("nothing here", "//bazel:crypto"));
    }
}
