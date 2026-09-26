//! What a closure Bazel walked outside the index was derived through, and
//! whether a run moved it.
//!
//! Two answers the index stores are closures Bazel walked through targets
//! the index holds no rows for: the consumer table's leaf map
//! (`cambium_index::externals::Consumer::via`) and the leaves' round-trip
//! rows (`crate::round_trip`, `IndexRef::round_trip_via`). No splice
//! re-queries such a closure, since no package of the index owns it, so each
//! records what it ran through, in one spelling, and a run holds that record
//! against what moved in it. A hit means the stored answer may not be
//! Bazel's answer at head, and the caller asks again.
//!
//! - `//pkg`: a main-repo package on the closure (`//` for the root
//!   package). Moved when the run dirtied the package: a `BUILD` edit, or a
//!   `.bzl` it loads.
//! - `//pkg:file`: a main-repo file the closure's external packages load,
//!   which a `.bzl` defining a rule class or macro they use is. Moved when
//!   the change set names its path. No main-repo package need load it, so
//!   the package walk never sees it: `bazelbuild/rules_shell`'s
//!   `shell/private/sh_binary.bzl` defines the class of
//!   `@bazel_tools//tools/test:collect_coverage`, whose edge into the main
//!   repository is that class's implicit `_runfiles_dep`.
//! - `@name`: a repository, as Bazel spelled it. Moved when a repository the
//!   run moved goes by that name or its canonical form.

use std::collections::{BTreeMap, BTreeSet};

use cambium_index::IndexRef;

/// What moved in one run, in the terms a via entry is written in.
pub(crate) struct Moved<'a> {
    /// Main-repo packages the run dirtied, by name (`""` is the root).
    main_dirty: &'a BTreeSet<String>,
    /// Repo-relative paths the change set names.
    paths: &'a BTreeSet<String>,
    /// Repositories that moved, by the name the index knows them by.
    repos: &'a BTreeSet<String>,
    canonical: BTreeMap<&'a str, &'a str>,
}

impl<'a> Moved<'a> {
    pub(crate) fn new(
        base: IndexRef<'a>,
        main_dirty: &'a BTreeSet<String>,
        paths: &'a BTreeSet<String>,
        repos: &'a BTreeSet<String>,
    ) -> Self {
        let canonical = (0..base.num_repos())
            .map(|r| (base.repo_name(r), base.repo_canonical.get(r)))
            .collect();
        Self {
            main_dirty,
            paths,
            repos,
            canonical,
        }
    }

    /// The first entry of `via` that moved, if one did. A repository named
    /// `except` does not count: the caller answers its moving another way.
    pub(crate) fn hit<'v>(
        &self,
        via: impl IntoIterator<Item = &'v str>,
        except: Option<&str>,
    ) -> Option<&'v str> {
        via.into_iter().find(|v| self.moved(v, except))
    }

    fn moved(&self, v: &str, except: Option<&str>) -> bool {
        if let Some(rest) = v.strip_prefix("//") {
            return match rest.split_once(':') {
                Some(("", file)) => self.paths.contains(file),
                Some((pkg, file)) => self.paths.contains(&format!("{pkg}/{file}")),
                None => self.main_dirty.contains(rest),
            };
        }
        v.strip_prefix('@').is_some_and(|token| {
            self.repos.iter().any(|m| {
                Some(m.as_str()) != except
                    && crate::externals::repo_token_matches(
                        token,
                        m,
                        self.canonical.get(m.as_str()).copied().unwrap_or(""),
                    )
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Moved;
    use cambium_index::IndexBuilder;
    use std::collections::BTreeSet;

    fn set(xs: &[&str]) -> BTreeSet<String> {
        xs.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn each_spelling_moves_by_its_own_evidence() {
        let d = IndexBuilder::new([0; 16]).finish("c");
        let (dirty, paths, repos) = (
            set(&["shell/runfiles", ""]),
            set(&["shell/private/sh_binary.bzl", "defs.bzl"]),
            set(&["fmt"]),
        );
        let m = Moved::new(d.view(), &dirty, &paths, &repos);
        assert_eq!(m.hit(["//shell/runfiles"], None), Some("//shell/runfiles"));
        assert_eq!(m.hit(["//"], None), Some("//"));
        assert_eq!(m.hit(["//shell"], None), None);
        assert_eq!(
            m.hit(["//shell/private:sh_binary.bzl"], None),
            Some("//shell/private:sh_binary.bzl")
        );
        assert_eq!(m.hit(["//:defs.bzl"], None), Some("//:defs.bzl"));
        // A file of a dirty package is not the package.
        assert_eq!(m.hit(["//shell/runfiles:x.bzl"], None), None);
        assert_eq!(m.hit(["@fmt+"], None), Some("@fmt+"));
        assert_eq!(m.hit(["@fmt"], Some("fmt")), None);
        assert_eq!(m.hit(["@other"], None), None);
    }
}
