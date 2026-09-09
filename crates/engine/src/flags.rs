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
//! 3. **Starlark cambium cannot see.** The load closure stops at the first
//!    label into an external repository, so a transition inside *that*
//!    repository's `.bzl` files is invisible. For a setting that lives in an
//!    external repository — `@protobuf//bazel/flags:prefer_prebuilt_protoc` —
//!    the repository's own rules are exactly the code most likely to read it,
//!    so if the closure loads anything from that repository the run stays
//!    global. A setting nothing in the closure loads from cannot be read by a
//!    transition that any main-repo rule is analysed under.
//! 4. **No node.** If the label resolves to no node in the index, nothing
//!    depends on the setting through an edge; with (2) and (3) clean, the edit
//!    affects nothing at all.
//!
//! A flag's *value* changing does not change any rule's definition, so nothing
//! is re-queried: the resolved nodes join the direct-set seeds, like external
//! nodes do, and the walk does the rest.

use cambium_index::IndexRef;

use crate::bazelrc::LabelFlag;
use crate::externals::repo_token_matches;
use crate::query::PackageQuerier;

/// What a set of changed Starlark build settings reaches.
#[derive(Default, Debug)]
pub struct FlagReach {
    /// Flag nodes to seed the direct set with, as `(node id, label)`.
    pub nodes: Vec<(u32, String)>,
    /// Every flag that was resolved this way, as written in the rc file.
    pub flags: Vec<String>,
    /// Set when one of them could not be resolved soundly; the run is global.
    pub global: Option<String>,
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

/// The repository part of a canonical label: `@@rules_cc+//x:y` -> `rules_cc+`.
fn label_repo(label: &str) -> Option<&str> {
    let rest = label
        .strip_prefix("@@")
        .or_else(|| label.strip_prefix('@'))?;
    let (repo, _) = rest.split_once("//")?;
    (!repo.is_empty()).then_some(repo)
}

/// Whether the closure loads any `.bzl` out of the repository the setting lives
/// in — see the module doc's (3).
fn closure_loads_from(base: IndexRef<'_>, apparent: &str, canonical: &str) -> Option<String> {
    (0..base.bzl_ext_labels.len() as u32)
        .map(|e| base.bzl_ext_labels.get(e))
        .find(|l| label_repo(l).is_some_and(|t| repo_token_matches(t, apparent, canonical)))
        .map(str::to_string)
}

/// The node a setting's label denotes, if the index has one.
///
/// The main repository's label is looked up directly — `@envoy//bazel:crypto`
/// inside envoy is `//bazel:crypto`, which is how the index writes it. An external one is matched over the
/// contiguous run of `@`-prefixed labels — the same binary-searched run
/// `crate::externals::reach` walks — because the index writes repository names
/// canonically and the rc file writes them apparently.
fn node_of(base: IndexRef<'_>, p: &Parsed<'_>, main: bool, canonical: &str) -> Option<u32> {
    if main {
        return base.id_of(&format!("//{}", p.suffix));
    }
    let start = base
        .label_sorted
        .partition_point(|&id| base.label(id) < "@");
    base.label_sorted[start..].iter().copied().find(|&id| {
        let label = base.label(id);
        if base.is_tombstone(id) {
            return false;
        }
        match label.split_once("//") {
            Some((head, rest)) => {
                rest == p.suffix
                    && repo_token_matches(head.trim_start_matches('@'), p.repo, canonical)
            }
            None => false,
        }
    })
}

/// Resolves every changed Starlark build setting, or says why the run must stay
/// global.
pub fn resolve(base: IndexRef<'_>, querier: &dyn PackageQuerier, flags: &[LabelFlag]) -> FlagReach {
    let mut out = FlagReach::default();
    if flags.is_empty() {
        return out;
    }
    let Some(src) = querier.load_source() else {
        out.global = Some(reason(
            flags,
            "the row source cannot read Starlark, so no `.bzl` can be checked \
             for a transition that reads it",
        ));
        return out;
    };
    if !base.bzl_closure {
        out.global = Some(reason(
            flags,
            "the base index carries no static .bzl load closure to check for a \
             transition that reads it",
        ));
        return out;
    }
    let facts = cambium_index::read_module_facts(src);
    let mapping = querier
        .repo_mapping()
        .with_main_alias(&facts.module_name)
        .with_main_alias("__main__");

    for f in flags {
        let Some(p) = parse_label(&f.label) else {
            out.global = Some(reason(
                std::slice::from_ref(f),
                "is not a label cambium parses",
            ));
            return out;
        };
        let main = mapping.is_main(p.repo);
        if let Some(path) = bzl_witness(base, src, &p.suffix) {
            out.global = Some(reason(
                std::slice::from_ref(f),
                &format!(
                    "and `{path}` in the load closure names it — a transition or other \
                          Starlark read of the setting is not an edge in the graph"
                ),
            ));
            return out;
        }
        let canonical = if main {
            String::new()
        } else {
            mapping.canonical(p.repo).to_string()
        };
        if !main {
            if let Some(load) = closure_loads_from(base, p.repo, &canonical) {
                out.global = Some(reason(
                    std::slice::from_ref(f),
                    &format!(
                        "and the load closure reaches `{load}`, whose repository's own \
                              Starlark cambium cannot read for a transition on it"
                    ),
                ));
                return out;
            }
        }
        out.flags.push(f.label.clone());
        match node_of(base, &p, main, &canonical) {
            Some(id) => {
                tracing::debug!(flag = %f.label, label = %base.label(id), "starlark build setting resolved");
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

fn reason(flags: &[LabelFlag], why: &str) -> String {
    let f = &flags[0];
    format!("{} --{} changed {why}", f.key.command, f.key.name)
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
