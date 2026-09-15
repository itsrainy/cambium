//! Reading `bazel dump --skyframe=deps`.
//!
//! The format is a block per node: the node's key on a column-0 line, then
//! its dependencies under indented `Group N:` headings, then a blank line.
//!
//! ```text
//! PACKAGE:src/tools/launcher/util
//!   Group 1:
//!     PACKAGE_LOOKUP:src/tools/launcher/util
//!
//!   Group 2:
//!     FILE:[/abs/root]/[src/tools/launcher/util/BUILD]
//! ```
//!
//! A node with no dependencies is a bare key line. The groups are Skyframe's
//! own request batching and carry no meaning for a reverse walk, so they are
//! dropped and the dependencies are one flat list.
//!
//! This parser is deliberately its own: another prototype branch is writing
//! one at `crates/bench/src/skyframe/dump.rs`, and two small parsers that
//! never have to agree are cheaper than one that two branches edit.

/// Calls `sink(key, deps)` once per block, in the dump's own order.
///
/// Both the key and the dependencies are handed over verbatim, including the
/// `KIND:` prefix; deciding which kinds matter is [`super::graph`]'s job.
pub fn parse_dump(text: &str, sink: &mut impl FnMut(&str, &[&str])) {
    let mut key: Option<&str> = None;
    let mut deps: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            let dep = line.trim();
            // `Group 1:` is the batch heading, not a dependency.
            if dep.is_empty() || is_group_heading(dep) {
                continue;
            }
            if key.is_some() {
                deps.push(dep);
            }
            continue;
        }
        // A column-0 line opens a block and closes the one before it.
        if let Some(k) = key.take() {
            sink(k, &deps);
        }
        deps.clear();
        // Bazel's own banner lines are not keys; a key always has a kind.
        if kind_of(line).is_some() {
            key = Some(line);
        }
    }
    if let Some(k) = key {
        sink(k, &deps);
    }
}

fn is_group_heading(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("Group ") else {
        return false;
    };
    let rest = rest.strip_suffix(':').unwrap_or(rest);
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// The `KIND` of a key, or `None` when the line is not a key at all.
///
/// A kind is upper-case ASCII with underscores, followed by a colon. That is
/// strict enough to reject Bazel's `WARNING: ...` and `INFO: ...` banners,
/// which the dump writes to stdout alongside the graph.
pub fn kind_of(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let (kind, rest) = line.split_at(colon);
    if kind.is_empty() || !kind.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
        return None;
    }
    match kind {
        // The banners, which are shaped exactly like a key.
        "WARNING" | "INFO" | "ERROR" | "DEBUG" => None,
        _ => Some((kind, &rest[1..])),
    }
}

/// Splits `[root]/[rel]` into its two halves.
///
/// Every filesystem key in the dump is written this way, so the absolute root
/// can be stripped and the stored graph is not tied to one machine's
/// checkout path. A key that is not shaped like this (Bazel writes
/// `[<absolute root>]/[/abs/path]` for a file outside any known root) still
/// splits, with the literal text as the root.
pub fn split_rooted(rest: &str) -> Option<(&str, &str)> {
    let inner = rest.strip_prefix('[')?;
    let close = inner.find("]/[")?;
    let (root, tail) = inner.split_at(close);
    let rel = tail.strip_prefix("]/[")?.strip_suffix(']')?;
    Some((root, rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture cut from a real `bazel dump --skyframe=deps` on the `bazel`
    /// subject under Bazel 9.2.0, with the checkout path shortened.
    const FIXTURE: &str = "\
WARNING: This information is intended for consumption by developers only.
FILE_STATE:[/root]/[docs/versions/8.7.0/advanced/BUILD.bazel]

PACKAGE:src/tools/launcher/util
  Group 1:
    PRECOMPUTED:lazy_macro_expansion_packages

  Group 2:
    PACKAGE_LOOKUP:src/tools/launcher/util

  Group 6:
    FILE:[/root]/[src/tools/launcher/util/BUILD]

  Group 13:
    GLOBS:<GlobsKey packageRoot = /root, packageIdentifier = src/tools/launcher/util, globRequests = [GlobRequest: ** FILES]>


DIRECTORY_LISTING:[/root]/[src/tools/execlog]
  Group 1:
    FILE:[/root]/[src/tools/execlog]

  Group 2:
    DIRECTORY_LISTING_STATE:[/root]/[src/tools/execlog]

";

    fn blocks(text: &str) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        parse_dump(text, &mut |k, d| {
            out.push((k.to_string(), d.iter().map(|s| s.to_string()).collect()))
        });
        out
    }

    #[test]
    fn a_block_is_a_key_and_a_flat_list_of_deps() {
        let out = blocks(FIXTURE);
        assert_eq!(out.len(), 3, "{out:#?}");

        // A bare key line is a node with no dependencies, not a dropped one.
        assert_eq!(
            out[0].0,
            "FILE_STATE:[/root]/[docs/versions/8.7.0/advanced/BUILD.bazel]"
        );
        assert!(out[0].1.is_empty());

        // The group headings are gone and the groups are one list.
        assert_eq!(out[1].0, "PACKAGE:src/tools/launcher/util");
        assert_eq!(out[1].1.len(), 4);
        assert_eq!(out[1].1[1], "PACKAGE_LOOKUP:src/tools/launcher/util");
        // A key whose own text contains brackets and commas survives whole.
        assert!(out[1].1[3].ends_with("globRequests = [GlobRequest: ** FILES]>"));

        assert_eq!(out[2].0, "DIRECTORY_LISTING:[/root]/[src/tools/execlog]");
        assert_eq!(out[2].1.len(), 2);
    }

    #[test]
    fn a_banner_line_is_not_a_key() {
        // The WARNING line opens no block, and does not swallow the one after.
        let out = blocks("WARNING: hello\nINFO: x\nPACKAGE:a\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "PACKAGE:a");
        assert_eq!(kind_of("WARNING: hello"), None);
        assert_eq!(kind_of("not a key"), None);
        assert_eq!(kind_of("PACKAGE:a"), Some(("PACKAGE", "a")));
        assert_eq!(
            kind_of("FILE_STATE:[/r]/[a/b]"),
            Some(("FILE_STATE", "[/r]/[a/b]"))
        );
    }

    #[test]
    fn a_group_heading_is_told_from_a_dep_that_starts_with_group() {
        assert!(is_group_heading("Group 12:"));
        assert!(!is_group_heading("GROUPS:x"));
        assert!(!is_group_heading("Group x:"));
    }

    #[test]
    fn a_rooted_key_splits_into_root_and_relative_path() {
        assert_eq!(split_rooted("[/a/b]/[c/d]"), Some(("/a/b", "c/d")));
        // Bazel's own spelling for a file under no known root.
        assert_eq!(
            split_rooted("[<absolute root>]/[/x/y]"),
            Some(("<absolute root>", "/x/y"))
        );
        assert_eq!(split_rooted("src/foo"), None);
    }
}
