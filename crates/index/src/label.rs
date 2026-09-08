//! Canonical label handling.
//!
//! Labels are stored exactly as Bazel emits them: `//pkg:name` for main-repo
//! targets and `@repo//pkg:name` (or `@@canonical//pkg:name` under bzlmod) for
//! external ones. Source files use the same form, with the name being the path
//! relative to the package: `//lib/core:sub/util.cc`.
//!
//! Parsing is deliberately minimal — we split on the *last* `:` and never
//! normalise, so byte-exact labels round-trip through the index even when they
//! contain unusual characters.

/// Returns the package part of a main-repo label, e.g. `//lib/core:core` -> `lib/core`.
/// Returns `None` for external labels or anything that is not a `//`-label.
pub fn main_repo_package(label: &str) -> Option<&str> {
    let rest = label.strip_prefix("//")?;
    Some(match rest.rfind(':') {
        Some(i) => &rest[..i],
        None => rest,
    })
}

/// Returns the target-name part of a label, e.g. `//lib/core:sub/x.cc` -> `sub/x.cc`.
pub fn target_name(label: &str) -> &str {
    match label.rfind(':') {
        Some(i) => &label[i + 1..],
        None => label,
    }
}

/// True if the label refers to a target outside the main repository.
pub fn is_external(label: &str) -> bool {
    label.starts_with('@')
}

/// Builds the canonical label for a source file at repo-relative `path` owned by
/// `package` (`""` for the root package).
///
/// ```
/// # use cambium_index::label::source_label;
/// assert_eq!(source_label("lib/core", "lib/core/sub/x.cc"), "//lib/core:sub/x.cc");
/// assert_eq!(source_label("", "README.md"), "//:README.md");
/// ```
pub fn source_label(package: &str, path: &str) -> String {
    // Callers guarantee `path` is under `package` (i.e. `path` starts with
    // `package` followed by '/'); we only debug_assert it since checking on
    // every call would defeat the point of a cheap label builder.
    debug_assert!(
        package.is_empty()
            || (path.len() > package.len()
                && path.starts_with(package)
                && path.as_bytes()[package.len()] == b'/'),
        "source_label: {path:?} is not under package {package:?}"
    );
    let name = if package.is_empty() {
        path
    } else {
        // +1 skips the '/' separator.
        &path[package.len() + 1..]
    };
    let mut s = String::with_capacity(2 + package.len() + 1 + name.len());
    s.push_str("//");
    s.push_str(package);
    s.push(':');
    s.push_str(name);
    s
}

/// Builds the canonical label for a target `name` in `package`.
pub fn rule_label(package: &str, name: &str) -> String {
    let mut s = String::with_capacity(2 + package.len() + 1 + name.len());
    s.push_str("//");
    s.push_str(package);
    s.push(':');
    s.push_str(name);
    s
}

/// Repo-relative path of the file a source-file label refers to.
/// Returns `None` for external labels.
pub fn source_path(label: &str) -> Option<String> {
    let pkg = main_repo_package(label)?;
    let name = target_name(label);
    Some(if pkg.is_empty() {
        name.to_string()
    } else {
        format!("{pkg}/{name}")
    })
}

/// Parent package path of `pkg`, or `None` for the root package.
pub fn parent_package(pkg: &str) -> Option<&str> {
    if pkg.is_empty() {
        None
    } else {
        Some(match pkg.rfind('/') {
            Some(i) => &pkg[..i],
            None => "",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_labels() {
        assert_eq!(main_repo_package("//lib/core:core"), Some("lib/core"));
        assert_eq!(main_repo_package("//:root"), Some(""));
        assert_eq!(main_repo_package("//lib/core"), Some("lib/core"));
        assert_eq!(main_repo_package("@abs//x:y"), None);
        assert_eq!(target_name("//lib:sub/a:b.cc"), "b.cc");
        assert!(is_external("@com_google_absl//absl:base"));
    }

    #[test]
    fn round_trips_source_paths() {
        for (pkg, path) in [("lib/core", "lib/core/a/b.cc"), ("", "README.md")] {
            let l = source_label(pkg, path);
            assert_eq!(source_path(&l).as_deref(), Some(path));
        }
    }

    #[test]
    fn walks_parents() {
        assert_eq!(parent_package("a/b/c"), Some("a/b"));
        assert_eq!(parent_package("a"), Some(""));
        assert_eq!(parent_package(""), None);
    }
}
