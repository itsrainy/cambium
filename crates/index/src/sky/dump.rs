//! Reading `bazel dump --skyframe=deps`.
//!
//! The format is a block per node: the node's key on a column-0 line, then its
//! dependencies under indented `Group N:` headings, with a blank line between
//! the groups and between the blocks. A node with no dependencies is a bare
//! key line. The groups are Skyframe's own request batching and carry no
//! meaning for a reverse walk, so they are dropped and the dependencies become
//! one flat list.
//!
//! ```text
//! PACKAGE:a
//!   Group 1:
//!     PACKAGE_LOOKUP:a
//!
//!   Group 2:
//!     FILE:[/abs/root]/[a/BUILD]
//! ```
//!
//! Bazel writes a `WARNING:` banner to the same stream. A banner is shaped
//! exactly like a key, so [`kind_of`] rejects the four banner words by name.

/// Calls `sink(key, deps)` once per block, in the dump's own order.
///
/// Both the key and the dependencies are handed over verbatim, including the
/// `KIND:` prefix; deciding which kinds matter is [`super::Kind`]'s job.
pub fn parse_dump(text: &str, sink: &mut impl FnMut(&str, &[&str])) {
    let mut key: Option<&str> = None;
    let mut deps: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            let dep = line.trim();
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

/// The `KIND` of a key and the text after the colon, or `None` when the line
/// is not a key at all.
///
/// A kind is upper-case ASCII with underscores, followed by a colon. That is
/// strict enough to reject Bazel's own `WARNING:` and `INFO:` banners, which
/// the dump writes to the same stream as the graph.
pub fn kind_of(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let (kind, rest) = line.split_at(colon);
    if kind.is_empty() || !kind.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
        return None;
    }
    match kind {
        "WARNING" | "INFO" | "ERROR" | "DEBUG" => None,
        _ => Some((kind, &rest[1..])),
    }
}

/// Splits `[root]/[rel]` into its two halves.
///
/// Every filesystem key in the dump is written this way, so the absolute root
/// can be lifted out and interned once instead of being repeated in every key.
/// A key Bazel wrote for a path under no known root (`[<absolute root>]/[/x]`)
/// splits too, with the literal text as the root.
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

    /// Cut from a real `bazel dump --skyframe=deps` on Bazel 9.2.0, with the
    /// checkout path shortened.
    const FIXTURE: &str = "\
WARNING: This information is intended for consumption by developers only.
FILE_STATE:[/root]/[a/one.txt]

PACKAGE:a
  Group 1:
    PACKAGE_LOOKUP:a

  Group 2:
    FILE:[/root]/[a/BUILD]

  Group 3:
    GLOBS:<GlobsKey packageRoot = /root, packageIdentifier = a, globRequests = [GlobRequest: *.txt FILES]>


DIRECTORY_LISTING:[/root]/[a]
  Group 1:
    DIRECTORY_LISTING_STATE:[/root]/[a]

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
        assert_eq!(out[0].0, "FILE_STATE:[/root]/[a/one.txt]");
        assert!(out[0].1.is_empty());
        // The group headings are gone and the groups are one list.
        assert_eq!(out[1].0, "PACKAGE:a");
        assert_eq!(out[1].1.len(), 3);
        // A key whose own text holds brackets and commas survives whole.
        assert!(out[1].1[2].ends_with("globRequests = [GlobRequest: *.txt FILES]>"));
        assert_eq!(out[2].1.len(), 1);
    }

    #[test]
    fn a_banner_line_is_not_a_key() {
        let out = blocks("WARNING: hello\nINFO: x\nPACKAGE:a\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "PACKAGE:a");
        assert_eq!(kind_of("WARNING: hello"), None);
        assert_eq!(kind_of("not a key"), None);
        assert_eq!(kind_of("PACKAGE:a"), Some(("PACKAGE", "a")));
        // The root package's key has nothing after the colon.
        assert_eq!(kind_of("PACKAGE_LOOKUP:"), Some(("PACKAGE_LOOKUP", "")));
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
        // Bazel's own spelling for a path under no known root.
        assert_eq!(
            split_rooted("[<absolute root>]/[/x/y]"),
            Some(("<absolute root>", "/x/y"))
        );
        // The workspace root's own directory listing.
        assert_eq!(split_rooted("[/a]/[]"), Some(("/a", "")));
        assert_eq!(split_rooted("a"), None);
    }
}
