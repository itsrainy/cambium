//! Canonical label handling.
//!
//! Labels are stored exactly as Bazel emits them: `//pkg:name` for main-repo
//! targets and `@repo//pkg:name` (or `@@canonical//pkg:name` under bzlmod) for
//! external ones. Source files use the same form, with the name being the path
//! relative to the package: `//lib/core:sub/util.cc`.
//!
//! Two readers live here. The string functions at the top are deliberately
//! minimal — they split on the *last* `:` and never normalise, so byte-exact
//! labels round-trip through the index even when they contain unusual
//! characters. [`Label`] is the structured reading every Starlark scanner in
//! this crate shares: it takes a label apart once — repository as written,
//! package, target — and the callers ask it the questions they used to answer
//! with their own `strip_prefix("@@")` ladders.

use crate::loads::RepoMapping;

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

/// The repo-relative path of `name` inside `package`: `pkg/name`, or just
/// `name` in the root package. The one place the join is written.
pub fn join_path(package: &str, name: &str) -> String {
    if package.is_empty() {
        name.to_string()
    } else {
        format!("{package}/{name}")
    }
}

/// Repo-relative path of the file a source-file label refers to.
/// Returns `None` for external labels.
pub fn source_path(label: &str) -> Option<String> {
    let pkg = main_repo_package(label)?;
    Some(join_path(pkg, target_name(label)))
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

/// The repository a label names, as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repo<'a> {
    /// `//pkg:name`, or a relative `:name`: the repository the label is
    /// written in, which for everything this crate reads is the main one.
    Main,
    /// `@apparent//pkg:name`. `@//pkg:name` is `Apparent("")`, the main
    /// repository spelled out; which other names mean the main repository is
    /// the [`RepoMapping`]'s to say.
    Apparent(&'a str),
    /// `@@canonical//pkg:name`; `@@//pkg:name` is the main repository.
    Canonical(&'a str),
}

/// A label taken apart: `@@canonical//pkg:name`, `@apparent//pkg:name`,
/// `//pkg:name`, `:name`, or `//pkg` (which names `//pkg:pkg`).
///
/// Purely syntactic. Nothing here is validated beyond the shape — a package
/// with a leading `/`, a name that is a visibility token, a `%s` where a
/// name should be — because each reader has its own idea of what to reject
/// and the wide answer to fall back to; see [`crate::externals`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Label<'a> {
    repo: Repo<'a>,
    /// Everything after the `//`, or after the `:` of a relative label.
    rest: &'a str,
    package: &'a str,
    /// The name after `:`, when one was written.
    name: Option<&'a str>,
    relative: bool,
}

impl<'a> Label<'a> {
    /// Reads a label. `None` for a string that is not one: a bare file name,
    /// `@repo` with no `//`, or anything else that starts with neither `@`,
    /// `//` nor `:`.
    pub fn parse(label: &'a str) -> Option<Label<'a>> {
        if let Some(name) = label.strip_prefix(':') {
            return Some(Label {
                repo: Repo::Main,
                rest: name,
                package: "",
                name: Some(name),
                relative: true,
            });
        }
        let (repo, rest) = if let Some(r) = label.strip_prefix("@@") {
            let (c, rest) = r.split_once("//")?;
            (Repo::Canonical(c), rest)
        } else if let Some(r) = label.strip_prefix('@') {
            let (a, rest) = r.split_once("//")?;
            (Repo::Apparent(a), rest)
        } else {
            (Repo::Main, label.strip_prefix("//")?)
        };
        let (package, name) = match rest.split_once(':') {
            Some((p, n)) => (p, Some(n)),
            None => (rest, None),
        };
        Some(Label {
            repo,
            rest,
            package,
            name,
            relative: false,
        })
    }

    pub fn repo(&self) -> Repo<'a> {
        self.repo
    }

    /// The repository name as written: empty for `//`, the apparent or
    /// canonical name otherwise.
    pub fn repo_name(&self) -> &'a str {
        match self.repo {
            Repo::Main => "",
            Repo::Apparent(n) | Repo::Canonical(n) => n,
        }
    }

    /// Whether the label was written relative to its own package (`:name`),
    /// in which case [`Label::package`] is empty and the caller knows the
    /// package.
    pub fn is_relative(&self) -> bool {
        self.relative
    }

    /// The text after `//` (or after the `:` of a relative label), untouched.
    pub fn rest(&self) -> &'a str {
        self.rest
    }

    pub fn package(&self) -> &'a str {
        self.package
    }

    /// The target name after `:`, when one was written. `//pkg` has none.
    pub fn explicit_name(&self) -> Option<&'a str> {
        self.name
    }

    /// The target name: what follows `:`, or for the `//pkg` shorthand the
    /// last segment of the package, since `//pkg` means `//pkg:pkg`.
    pub fn name(&self) -> &'a str {
        match self.name {
            Some(n) => n,
            None => self.package.rsplit('/').next().unwrap_or(self.package),
        }
    }

    /// The repo-relative path the label names as a file: `pkg/name`.
    pub fn path(&self) -> String {
        join_path(self.package, self.name())
    }

    /// The canonical name of the label's repository under `m`: empty for the
    /// main repository, however it was spelled.
    pub fn canonical_repo<'m>(&self, m: &'m RepoMapping) -> &'m str
    where
        'a: 'm,
    {
        match self.repo {
            Repo::Main => "",
            Repo::Canonical(c) => c,
            Repo::Apparent(a) => m.canonical(a),
        }
    }

    /// Whether the label names the main repository under `m`: written as
    /// `//`, `@//` or `@@//`, or under an apparent name the mapping says is
    /// the main one.
    pub fn is_main(&self, m: &RepoMapping) -> bool {
        self.canonical_repo(m).is_empty()
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

    #[test]
    fn a_label_is_taken_apart_once() {
        let m = RepoMapping::from_dump("{\"\":\"\",\"demo\":\"\",\"rules_cc\":\"rules_cc+\"}");

        let l = Label::parse("@@rules_cc+//cc:defs.bzl").unwrap();
        assert_eq!(l.repo(), Repo::Canonical("rules_cc+"));
        assert_eq!(l.repo_name(), "rules_cc+");
        assert_eq!((l.package(), l.explicit_name()), ("cc", Some("defs.bzl")));
        assert_eq!(l.rest(), "cc:defs.bzl");
        assert_eq!(l.canonical_repo(&m), "rules_cc+");
        assert!(!l.is_main(&m));

        let l = Label::parse("@rules_cc//cc:defs.bzl").unwrap();
        assert_eq!(l.repo(), Repo::Apparent("rules_cc"));
        assert_eq!(l.canonical_repo(&m), "rules_cc+");
        assert!(!Label::parse("@unmapped//x:y").unwrap().is_main(&m));

        // The main repository, every way it can be spelled.
        for main in ["//x:y", "@//x:y", "@@//x:y", "@demo//x:y"] {
            let l = Label::parse(main).unwrap();
            assert!(l.is_main(&m), "{main}");
            assert_eq!(l.canonical_repo(&m), "", "{main}");
            assert_eq!(l.path(), "x/y", "{main}");
            assert!(!l.is_relative());
        }
        assert_eq!(Label::parse("//x:y").unwrap().repo(), Repo::Main);
        assert_eq!(Label::parse("@//x:y").unwrap().repo(), Repo::Apparent(""));

        // `//pkg` is `//pkg:pkg`; `//:name` is in the root package.
        let l = Label::parse("//a/pkg").unwrap();
        assert_eq!(
            (l.package(), l.explicit_name(), l.name()),
            ("a/pkg", None, "pkg")
        );
        assert_eq!(l.path(), "a/pkg/pkg");
        let l = Label::parse("//:README").unwrap();
        assert_eq!((l.package(), l.name()), ("", "README"));
        assert_eq!(l.path(), "README");
        assert_eq!(Label::parse("//").unwrap().name(), "");

        // Relative to the writing package, which the caller supplies.
        let l = Label::parse(":defs.bzl").unwrap();
        assert!(l.is_relative());
        assert_eq!(
            (l.package(), l.name(), l.rest()),
            ("", "defs.bzl", "defs.bzl")
        );

        // Not labels.
        for not in ["defs.bzl", "@rules_cc", "@@rules_cc+", "", "https://x//y"] {
            assert!(Label::parse(not).is_none(), "{not}");
        }
    }
}
