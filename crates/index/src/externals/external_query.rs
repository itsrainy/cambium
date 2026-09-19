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
//! an attribute counts only when its value is exactly one of the directories
//! the boundary walk already found, which is the claim the mapping is about. A
//! value that is absolute, that points outside the tree, or that names a
//! directory which is not a repository of its own says nothing about any
//! boundary and is dropped rather than guessed at.
//!
//! The attribute is not required to be called `path`. envoy declares
//! `@envoy_api` with a rule of its own:
//!
//! ```text
//! _default_envoy_api(name = "envoy_api", reldir = "api")
//! ```
//!
//! whose implementation symlinks `ctx.path(ctx.attr.envoy_root).dirname
//! .get_child(ctx.attr.reldir)`, so the directory is named by `reldir` and a
//! parser that reads only `path` learns nothing. Any *string* attribute whose
//! value is a boundary directory is therefore taken as naming it, `name`
//! excepted: a repository called `api` is not thereby the directory `api`.
//! A coincidental match can only claim that a repository is built from a
//! directory that is a repository of its own, which over-approximates what
//! that directory's changes move and never under-approximates.

use std::collections::BTreeMap;

/// What one `//external:*` block says about a repository's shape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExternalRepo {
    /// The main-repo directory the repository is, when an attribute named
    /// one; repo-relative, no trailing slash.
    pub dir: Option<String>,
    /// The `build_file` label as written, or the empty string when the block
    /// carried `build_file_content` instead. `None` when it had neither.
    ///
    /// Unlike the directory this is *not* checked against the boundaries: a
    /// `build_file` is a label, never a boundary directory, and it is read
    /// for what it is rather than matched against anything.
    pub build_file: Option<String>,
}

/// Repository name -> what the `--output=build` output says it is, for every
/// rule carrying a string attribute whose value is exactly one of
/// `boundaries` (repo-relative, no trailing slash), or a `build_file` /
/// `build_file_content` of its own.
///
/// The output is a sequence of `kind(\n  attr = value,\n ...)` blocks with
/// one attribute per line, so a line reader is enough and no Starlark parse is
/// needed. A block whose `name` is missing or is not a plain string literal,
/// or which names no boundary, contributes nothing. `path` wins when a block
/// names a boundary twice; otherwise the first such attribute does.
pub fn external_dir_repos(text: &str, boundaries: &[String]) -> BTreeMap<String, ExternalRepo> {
    let mut out = BTreeMap::new();
    let mut name: Option<String> = None;
    let mut path: Option<String> = None;
    let mut other: Option<String> = None;
    let mut build_file: Option<String> = None;
    macro_rules! flush {
        () => {
            if let Some(n) = name.take() {
                let r = ExternalRepo {
                    dir: path.take().or(other.take()),
                    build_file: build_file.take(),
                };
                if r != ExternalRepo::default() {
                    out.insert(n, r);
                }
            }
            name = None;
            path = None;
            other = None;
            build_file = None;
        };
    }
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with(')') {
            flush!();
            continue;
        }
        if t.ends_with('(') && !line.starts_with(' ') {
            // A new rule block: whatever an unterminated previous one held is
            // not carried into it.
            flush!();
            continue;
        }
        let Some((key, value)) = t.split_once(" = ") else {
            continue;
        };
        let Some(v) = string_literal(value) else {
            continue;
        };
        // `generator_name` is what a macro-generated rule carries; the
        // repository's own name is `name`, and only that. Neither the name nor
        // the bookkeeping attributes say anything about a directory, whatever
        // they happen to spell.
        if key == "name" {
            name = Some(v);
            continue;
        }
        if key == "repo_mapping" || key.starts_with("generator_") {
            continue;
        }
        if key == "build_file" {
            build_file = Some(v);
            continue;
        }
        if key == "build_file_content" {
            build_file.get_or_insert_with(String::new);
            continue;
        }
        let dir = v.trim_start_matches("./").trim_end_matches('/').to_string();
        if !boundaries.contains(&dir) {
            continue;
        }
        if key == "path" {
            path = Some(dir);
        } else if other.is_none() {
            other = Some(dir);
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

    fn dir<'a>(m: &'a BTreeMap<String, ExternalRepo>, n: &str) -> Option<&'a str> {
        m.get(n)?.dir.as_deref()
    }

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
        assert_eq!(dir(&m, "xla"), Some("third_party/xla"));
        assert_eq!(dir(&m, "tsl"), Some("third_party/tsl"));
        assert_eq!(m.len(), 2);
    }

    /// envoy names the directory `reldir`, not `path`, and a repository whose
    /// *name* happens to be a boundary directory is not that directory.
    #[test]
    fn any_attribute_but_the_name_can_name_the_directory() {
        let text = concat!(
            "_default_envoy_api(\n",
            "  name = \"envoy_api\",\n",
            "  envoy_root = \"@envoy//:VERSION.txt\",\n",
            "  reldir = \"api\",\n",
            ")\n",
            "local_repository(\n",
            "  name = \"api\",\n",
            "  urls = [\"https://example.test/api.tar.gz\"],\n",
            ")\n",
        );
        let m = external_dir_repos(text, &["api".to_string()]);
        assert_eq!(dir(&m, "envoy_api"), Some("api"));
        assert_eq!(m.len(), 1);
    }

    /// The overlay half: `build_file` is a label, never a boundary, so it is
    /// read for what it is and the block yields a repository even though it
    /// names no directory at all.
    #[test]
    fn a_build_file_is_read_without_naming_a_directory() {
        let text = concat!(
            "new_local_repository(\n",
            "  name = \"quiche\",\n",
            "  build_file = \"@envoy//bazel/external:quiche.BUILD\",\n",
            ")\n",
            "http_archive(\n",
            "  name = \"inline\",\n",
            "  build_file_content = \"cc_library(name = 'x')\",\n",
            ")\n",
        );
        let m = external_dir_repos(text, &[]);
        assert_eq!(
            m["quiche"].build_file.as_deref(),
            Some("@envoy//bazel/external:quiche.BUILD")
        );
        assert_eq!(m["quiche"].dir, None);
        assert_eq!(m["inline"].build_file.as_deref(), Some(""));
        assert_eq!(m.len(), 2);
    }

    /// A rule's bookkeeping attributes are not directory claims either.
    #[test]
    fn the_generator_attributes_are_not_a_directory_claim() {
        let text = concat!(
            "tf_vendored(\n",
            "  name = \"x\",\n",
            "  generator_name = \"api\",\n",
            "  generator_function = \"api\",\n",
            "  repo_mapping = \"api\",\n",
            ")\n",
        );
        assert!(external_dir_repos(text, &["api".to_string()]).is_empty());
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
