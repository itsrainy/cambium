//! Reading `bazel query '//external:*' --output=build`: which repository is
//! which main-repo directory.
//!
//! # Why Bazel has to be asked at all
//!
//! Everything else in this module derives its answer from the text of the
//! workspace files. That works for the shapes whose `path` is written down:
//! a `local_path_override`, a literal `local_repository`. It does not work for
//! tensorflow, which declares `third_party/xla` as the repository `@xla`
//! through a repository rule of its own:
//!
//! ```text
//! tf_vendored(name = "xla", path = "third_party/xla")
//! ```
//!
//! whose implementation is `ctx.symlink(ctx.path(ctx.attr._root).dirname
//! .get_child(ctx.attr.path), ".")`. The directory is assembled at load time
//! out of two attributes and a parent walk, and no scan of the `.bzl` can be
//! sure where it lands. But the tree is a Bazel repository — it carries a
//! `WORKSPACE` of its own, which is why `//...` stops at it — and Bazel, which
//! has already evaluated the rule, will print the attributes it ran with.
//!
//! So the cold build asks, and this is the parser. It is deliberately narrow:
//! a rule's `path` attribute counts only when it is one of the directories the
//! boundary walk already found, which is the claim the mapping is about. A
//! `path` that is absolute, that points outside the tree, or that names a
//! directory which is not a repository of its own says nothing about any
//! boundary and is dropped rather than guessed at.

use std::collections::BTreeMap;

/// Repository name -> the main-repo directory it is, for every rule in
/// `--output=build` output whose `path` attribute is exactly one of
/// `boundaries` (repo-relative, no trailing slash).
///
/// The output is a sequence of `kind(\n  attr = value,\n ...)` blocks with
/// one attribute per line, so a line reader is enough and no Starlark parse is
/// needed. A block whose `name` or `path` is missing, is not a plain string
/// literal, or whose `path` is not a boundary contributes nothing.
pub fn external_dir_repos(text: &str, boundaries: &[String]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut name: Option<String> = None;
    let mut path: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with(')') {
            if let (Some(n), Some(p)) = (name.take(), path.take()) {
                if boundaries.contains(&p) {
                    out.insert(n, p);
                }
            }
            name = None;
            path = None;
            continue;
        }
        if t.ends_with('(') && !line.starts_with(' ') {
            // A new rule block: whatever an unterminated previous one held is
            // not carried into it.
            name = None;
            path = None;
            continue;
        }
        let Some((key, value)) = t.split_once(" = ") else {
            continue;
        };
        let Some(v) = string_literal(value) else {
            continue;
        };
        match key {
            // `generator_name` is what a macro-generated rule carries; the
            // repository's own name is `name`, and only that.
            "name" => name = Some(v),
            "path" => path = Some(v.trim_end_matches('/').to_string()),
            _ => {}
        }
    }
    out
}

/// `"third_party/xla",` -> `third_party/xla`. Only a plain double-quoted
/// literal with nothing else on the line; anything else is not a path this
/// parser is willing to claim.
fn string_literal(value: &str) -> Option<String> {
    let v = value.trim().trim_end_matches(',').trim();
    let inner = v.strip_prefix('"')?.strip_suffix('"')?;
    if inner.contains('"') || inner.contains('\\') {
        return None;
    }
    Some(inner.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = r#"# /ws/tensorflow/workspace3.bzl:8:12
tf_vendored(
  name = "xla",
  generator_name = "xla",
  path = "third_party/xla",
  visibility = ["//visibility:public"],
)
# /ws/WORKSPACE:3:17
local_repository(
  name = "tsl",
  path = "third_party/tsl/",
)
# /ws/WORKSPACE:9:14
http_archive(
  name = "rules_cc",
  urls = ["https://example.test/rules_cc.tar.gz"],
)
# /ws/WORKSPACE:14:17
local_repository(
  name = "elsewhere",
  path = "/opt/elsewhere",
)
# /ws/WORKSPACE:20:17
local_repository(
  name = "not_a_boundary",
  path = "tools/build",
)
"#;

    fn boundaries() -> Vec<String> {
        vec!["third_party/tsl".to_string(), "third_party/xla".to_string()]
    }

    #[test]
    fn reads_the_rules_whose_path_is_a_boundary() {
        let m = external_dir_repos(OUT, &boundaries());
        assert_eq!(m.get("xla").map(String::as_str), Some("third_party/xla"));
        assert_eq!(m.get("tsl").map(String::as_str), Some("third_party/tsl"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn a_path_that_is_not_a_boundary_says_nothing() {
        assert!(external_dir_repos(OUT, &["tools".to_string()]).is_empty());
        assert!(external_dir_repos(OUT, &[]).is_empty());
    }

    #[test]
    fn an_unreadable_attribute_is_not_guessed_at() {
        let text = concat!(
            "local_repository(\n",
            "  name = \"a\",\n",
            "  path = _computed,\n",
            ")\n",
            "local_repository(\n",
            "  path = \"third_party/xla\",\n",
            ")\n",
        );
        assert!(external_dir_repos(text, &boundaries()).is_empty());
    }

    #[test]
    fn empty_output_is_no_answer_rather_than_a_wrong_one() {
        assert!(external_dir_repos("", &boundaries()).is_empty());
        assert!(external_dir_repos("ERROR: no such package\n", &boundaries()).is_empty());
    }
}
