//! Which external repositories a change set dirties, from Bazel's own
//! repository marker files.
//!
//! This is the `rdeps` oracle's second root source (see the module comment
//! of [`super::rdeps`] for why it exists). Nothing here reads cambium's
//! tables: the only inputs are the change set's paths and the files Bazel
//! writes under `<output_base>/external/`.
//!
//! ## The marker file
//!
//! Bazel 7 and 8 write `<output_base>/external/@<canonical>.marker` for every
//! repository they fetch, and consult it to decide whether a later build must
//! fetch again (`RepositoryDelegatorFunction.DigestWriter` and
//! `RepoRecordedInput` in Bazel 8.7.0). The grammar:
//!
//! ```text
//! <rule key>                                   first line: a hash of the rule's definition
//! <escaped key> <escaped value>                one line per recorded input, sorted
//! ```
//!
//! Escaping replaces `\` with `\\`, a newline with `\n` and a space with
//! `\s`; `\0` is a null value. The key is `<KIND>:<rest>`, split at the first
//! colon, and there are five kinds:
//!
//! - `FILE:<path>`: a file (or directory, or absent path) the repository rule
//!   read or watched; the value is its digest or its type. A patch, a
//!   `build_file`, a lock file, a template.
//! - `DIRENTS:<path>`: the entry list of one directory, not its contents.
//! - `DIRTREE:<path>`: a whole directory tree, contents included.
//! - `ENV:<name>`: an environment variable. No path; ignored here.
//! - `REPO_MAPPING:<source>,<apparent>`: a repository mapping lookup. It
//!   changes with the module graph, not with a file, and is ignored here.
//!
//! `<path>` is `@@<canonical>//<relative path>` for a file inside the
//! workspace or inside another repository (`@@//` is the main repository;
//! Bazel also parses the one-`@` and no-`@` spellings, and so does this), or
//! an absolute filesystem path for anything else. It is a path, not a label:
//! `@@//third_party/llvm/build.patch`, no colon.
//!
//! ## Matching a change set
//!
//! A repository is dirty when a changed path is among its inputs:
//!
//! - `FILE` matches the changed path exactly.
//! - `DIRTREE` matches any changed path at or under the directory.
//! - `DIRENTS` matches a changed path directly in the directory that was
//!   added or deleted; a content edit does not change a directory listing.
//!   Without the added-or-deleted set, any changed child matches.
//! - An absolute path inside the checkout is read as the main-repository
//!   path it is.
//!
//! A repository whose inputs name a file of a dirty repository is dirty too,
//! to a fixpoint: xla's `@llvm-project` is generated from `@llvm-raw`, and
//! its marker names `FILE:@@+third_party_ext+llvm-raw//llvm/CMakeLists.txt`
//! and a `DIRENTS` for most of its directories, while the patches that changed
//! are inputs of `@llvm-raw` only. Once a repository is refetched any of its
//! files may differ, so every recorded input in it counts, whatever its kind.
//!
//! An in-tree repository (a `local_repository` or `local_path_override`
//! Bazel symlinks into the workspace) records no inputs of its own: its files
//! are source files and `external/<name>` is a symlink to them. Such a
//! symlink is followed here only to translate a changed path into the
//! repository's own spelling, so that a marker naming `@@envoy_api+//x` is
//! matched by a change to `api/x`. A repository rule that symlinks
//! individual children instead of the directory is not followed.
//!
//! ## What a marker cannot say
//!
//! A repository that was never fetched on this output base has no marker,
//! and a marker is only as recent as the last fetch or up-to-date check of
//! its repository. The oracle reads the markers twice to bound that (see
//! [`super::rdeps`]); a repository fetched on this output base for another
//! revision, and never checked since, keeps that revision's inputs.
//!
//! A marker also records only what the repository rule itself read. A
//! module extension's file inputs (a `requirements_lock` that `pip.parse`
//! reads) belong to the extension's evaluation, not to the markers of the
//! repositories it generates, and a change to a repository's definition
//! (its attributes, or the `.bzl` that defines its rule) changes the rule
//! key on the first line, which is not an input. Neither is seen here.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The kinds of recorded input that name a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    File,
    Dirents,
    DirTree,
}

/// Where a recorded input points.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Place {
    /// Inside a repository, by canonical name; `""` is the main repository.
    Repo { name: String, path: String },
    /// An absolute filesystem path outside every repository Bazel knows.
    Absolute(PathBuf),
}

/// One path-valued line of a marker file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Input {
    pub kind: Kind,
    pub place: Place,
}

/// One repository's marker file, reduced to the inputs that name a path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Marker {
    pub inputs: Vec<Input>,
    /// `ENV` and `REPO_MAPPING` lines: read and set aside.
    pub other: usize,
}

/// A marker line this parser could not read. Bazel treats one as "refetch";
/// the oracle records the repository instead of guessing its inputs.
#[derive(Debug, PartialEq, Eq)]
pub struct Unreadable(pub String);

/// Undoes `RepositoryDelegatorFunction.escape`: `\s` a space, `\n` a newline,
/// `\` before anything else that character. `\0` (a null value) comes back
/// as the empty string, which no path-valued key ever is.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            match c {
                'n' => out.push('\n'),
                's' => out.push(' '),
                '0' => {}
                other => out.push(other),
            }
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// `RepoCacheFriendlyPath.parse`: `@@repo//rel`, `@repo//rel` or `//rel`
/// inside a repository, anything else an absolute path. One `@` or two, the
/// name is taken as canonical (`RepositoryName.create`), and so it is here.
pub fn parse_place(s: &str) -> Option<Place> {
    if s.starts_with('@') || s.starts_with("//") {
        let skip = if s.starts_with("@@") {
            2
        } else if s.starts_with('@') {
            1
        } else {
            0
        };
        let slash = s.find("//")?;
        if slash < skip {
            return None;
        }
        return Some(Place::Repo {
            name: s[skip..slash].to_string(),
            path: s[slash + 2..].trim_end_matches('/').to_string(),
        });
    }
    let p = Path::new(s);
    p.is_absolute().then(|| Place::Absolute(p.to_path_buf()))
}

/// Parses a marker file's text.
///
/// The first non-empty line is the rule key and says nothing about files. A
/// line whose kind this parser does not know, or whose path does not parse,
/// makes the whole marker unreadable, which is what Bazel does with it too.
pub fn parse_marker(text: &str) -> Result<Marker, Unreadable> {
    let mut lines = text.split('\n').filter(|l| !l.is_empty());
    if lines.next().is_none() {
        return Err(Unreadable("no rule key".into()));
    }
    let mut m = Marker::default();
    for line in lines {
        let Some((key, _value)) = line.split_once(' ') else {
            return Err(Unreadable(format!("no value: {line}")));
        };
        let key = unescape(key);
        let Some((kind, rest)) = key.split_once(':') else {
            return Err(Unreadable(format!("no kind: {key}")));
        };
        let kind = match kind {
            "FILE" => Kind::File,
            "DIRENTS" => Kind::Dirents,
            "DIRTREE" => Kind::DirTree,
            "ENV" | "REPO_MAPPING" => {
                m.other += 1;
                continue;
            }
            other => return Err(Unreadable(format!("unknown kind {other}"))),
        };
        let Some(place) = parse_place(rest) else {
            return Err(Unreadable(format!("unparsable path: {key}")));
        };
        m.inputs.push(Input { kind, place });
    }
    Ok(m)
}

/// Every marker under one output base, by canonical repository name.
#[derive(Clone, Debug, Default)]
pub struct Markers {
    pub by_repo: BTreeMap<String, Marker>,
    /// Repositories whose marker would not parse.
    pub unreadable: BTreeSet<String>,
    /// In-tree repositories: `external/<name>` is a symlink into the
    /// workspace, and this is the workspace-relative directory it points at
    /// (`""` for the workspace root).
    pub in_tree: BTreeMap<String, String>,
}

impl Markers {
    /// Reads `<output_base>/external/@*.marker`, and the `external/<name>`
    /// symlinks that point into `workspace`. A missing directory is an empty
    /// set, not an error: an output base that never fetched has no markers.
    pub fn read(output_base: &Path, workspace: &Path) -> Markers {
        let mut out = Markers::default();
        let dir = output_base.join("external");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return out;
        };
        let ws = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(repo) = name
                .strip_prefix('@')
                .and_then(|n| n.strip_suffix(".marker"))
            {
                // Bazel writes markers as ISO-8859-1; a lossy read keeps
                // every ASCII path intact, which is all a label can be.
                let Ok(bytes) = std::fs::read(e.path()) else {
                    continue;
                };
                match parse_marker(&String::from_utf8_lossy(&bytes)) {
                    Ok(m) => {
                        out.by_repo.insert(repo.to_string(), m);
                    }
                    Err(_) => {
                        out.unreadable.insert(repo.to_string());
                    }
                }
                continue;
            }
            let is_link = e.file_type().map(|t| t.is_symlink()).unwrap_or(false);
            if !is_link {
                continue;
            }
            let Ok(target) = std::fs::canonicalize(e.path()) else {
                continue; // dangling: the stale-worktree case, not ours
            };
            if let Ok(rel) = target.strip_prefix(&ws) {
                if let Some(rel) = rel.to_str() {
                    out.in_tree.insert(name.to_string(), rel.to_string());
                }
            }
        }
        out
    }

    /// The union of two reads: a repository's inputs from both, so an input
    /// either read recorded counts.
    pub fn union(mut self, other: Markers) -> Markers {
        for (repo, m) in other.by_repo {
            let e = self.by_repo.entry(repo).or_default();
            for i in m.inputs {
                if !e.inputs.contains(&i) {
                    e.inputs.push(i);
                }
            }
            e.other = e.other.max(m.other);
        }
        self.unreadable.extend(other.unreadable);
        self.in_tree.extend(other.in_tree);
        self
    }
}

/// A repository the change set dirties, and why.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirtyRepository {
    /// Canonical name, as the marker file is named.
    pub name: String,
    /// What made it dirty, at most [`MAX_VIA`] of them: a changed path
    /// (workspace-relative), or `@@<repository>` for a dirty repository one
    /// of whose files it records.
    pub via: Vec<String>,
}

/// How many reasons a [`DirtyRepository`] keeps.
pub const MAX_VIA: usize = 8;

/// The change set, as the matcher needs it.
pub struct Changes<'a> {
    /// Every changed path, workspace-relative.
    pub changed: &'a BTreeSet<String>,
    /// The paths added or deleted, for `DIRENTS`; `None` when unknown, and
    /// then any changed child of a listed directory matches.
    pub added_or_deleted: Option<&'a BTreeSet<String>>,
}

/// The repositories whose recorded inputs a change set touches, directly or
/// through another dirty repository, sorted by name.
pub fn dirty(markers: &Markers, changes: &Changes<'_>, workspace: &Path) -> Vec<DirtyRepository> {
    // Every changed path in each repository's own spelling: the main
    // repository's, and an in-tree repository's for a path under it.
    let mut changed: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut spelled: Vec<(String, String, &str)> = Vec::new(); // (repo, path, original)
    for p in changes.changed {
        spelled.push((String::new(), p.clone(), p.as_str()));
        for (repo, dir) in &markers.in_tree {
            let rest = if dir.is_empty() {
                Some(p.as_str())
            } else {
                p.strip_prefix(dir.as_str())
                    .and_then(|r| r.strip_prefix('/'))
            };
            if let Some(rest) = rest {
                spelled.push((repo.clone(), rest.to_string(), p.as_str()));
            }
        }
    }
    for (repo, path, _) in &spelled {
        changed.entry(repo.as_str()).or_default().push(path.clone());
    }
    let structural: Option<BTreeSet<(String, String)>> = changes.added_or_deleted.map(|s| {
        spelled
            .iter()
            .filter(|(_, _, orig)| s.contains(*orig))
            .map(|(r, p, _)| (r.clone(), p.clone()))
            .collect()
    });
    let original: BTreeMap<(String, String), &str> = spelled
        .iter()
        .map(|(r, p, o)| ((r.clone(), p.clone()), *o))
        .collect();
    let ws = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());

    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (repo, m) in &markers.by_repo {
        for input in &m.inputs {
            let (name, path) = match &input.place {
                Place::Repo { name, path } => (name.as_str(), path.clone()),
                Place::Absolute(abs) => {
                    let abs = std::fs::canonicalize(abs).unwrap_or_else(|_| abs.clone());
                    match abs.strip_prefix(&ws).ok().and_then(|r| r.to_str()) {
                        Some(rel) => ("", rel.to_string()),
                        None => continue,
                    }
                }
            };
            let Some(paths) = changed.get(name) else {
                continue;
            };
            for c in paths {
                let hit = match input.kind {
                    Kind::File => *c == path,
                    Kind::DirTree => {
                        path.is_empty()
                            || *c == path
                            || c.strip_prefix(path.as_str())
                                .is_some_and(|r| r.starts_with('/'))
                    }
                    Kind::Dirents => {
                        parent(c) == path
                            && structural
                                .as_ref()
                                .is_none_or(|s| s.contains(&(name.to_string(), c.clone())))
                    }
                };
                if hit {
                    let o = original
                        .get(&(name.to_string(), c.clone()))
                        .copied()
                        .unwrap_or(c.as_str());
                    out.entry(repo.clone()).or_default().insert(o.to_string());
                }
            }
        }
    }

    // The chain: a repository recording any file of a dirty repository is
    // dirty, to a fixpoint.
    let mut naming: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (repo, m) in &markers.by_repo {
        for input in &m.inputs {
            if let Place::Repo { name, .. } = &input.place {
                if !name.is_empty() && name != repo {
                    naming
                        .entry(name.as_str())
                        .or_default()
                        .insert(repo.as_str());
                }
            }
        }
    }
    let mut work: Vec<String> = out.keys().cloned().collect();
    while let Some(r) = work.pop() {
        let Some(users) = naming.get(r.as_str()) else {
            continue;
        };
        for u in users {
            let fresh = !out.contains_key(*u);
            out.entry(u.to_string())
                .or_default()
                .insert(format!("@@{r}"));
            if fresh {
                work.push(u.to_string());
            }
        }
    }

    out.into_iter()
        .map(|(name, via)| DirtyRepository {
            name,
            via: via.into_iter().take(MAX_VIA).collect(),
        })
        .collect()
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// `@@<name>//...:*`: every target of one repository, the root a refetch of it
/// stands for.
pub fn root_pattern(name: &str) -> String {
    format!("@@{name}//...:*")
}

/// Is `label` a target of the repository `canonical`?
///
/// `bazel query --output=label` prints a repository by its apparent name
/// where the main repository's mapping has one (`@llvm-project//llvm:Support`
/// for `+llvm_extension+llvm-project`), so this accepts the canonical name
/// and the apparent spellings bzlmod derives from it: the last `+`- or
/// `~`-separated segment, or the module name before a trailing `+` or `~`.
/// It only decides which labels are left out of the `missing_external`
/// listing; it never touches the verdict.
pub fn label_in_repo(label: &str, canonical: &str) -> bool {
    let Some(rest) = label.strip_prefix('@') else {
        return false;
    };
    let rest = rest.strip_prefix('@').unwrap_or(rest);
    let Some((repo, _)) = rest.split_once("//") else {
        return false;
    };
    if repo == canonical {
        return true;
    }
    let module = canonical.trim_end_matches(['+', '~']);
    if module != canonical && repo == module {
        return true;
    }
    canonical
        .rsplit(['+', '~'])
        .next()
        .is_some_and(|last| !last.is_empty() && last == repo)
}

/// The Bazel output base the oracle's queries run on: the `--output_base`
/// startup flag when there is one (the bench always passes it), else what
/// `bazel info output_base` says.
pub fn output_base(bazel: &cambium_bazel::Bazel) -> Option<PathBuf> {
    bazel
        .startup_flags()
        .iter()
        .find_map(|f| f.strip_prefix("--output_base="))
        .map(PathBuf::from)
        .or_else(|| bazel.output_base().map(PathBuf::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xla at `5244879ec449`, `@+third_party_ext+llvm-raw.marker`, verbatim.
    const LLVM_RAW: &str = "e2ec2e1d432946ec63293d94f938b2c809b03fbd6e97c22ddd2d4a61a381ed1f
FILE:@@//third_party/llvm/build.patch 9d524565d22a493f3622decd460679e17101d94057f4e81ef2422155d9ff8f04
FILE:@@//third_party/llvm/generated.patch bc6eb7761f75fa3ddf5954e7c774cc663f61e48a7c668ccbd77e60aa23ad25e0
FILE:@@//third_party/llvm/lit_test.patch d9d4832a1a47d345b5300a8d73fc4922a432245e1a92976c3e12b2d5d845a356
FILE:@@//third_party/llvm/platforms_config.patch ff09f51989328b4f8bcf4e04fa8b7eef8055b359ca9bc925566ce0980bb13308
FILE:@@//third_party/llvm/run_lit.patch 95317b48c95176def8dd1a175073464cd6623967a4ee5389a103f7fe7986a42e
FILE:@@//third_party/llvm/toolchains.patch dba1337e7eabdcf4ef25a077e408a86ba02f17878f9f23635acdebea8a766aaf
";

    /// xla, `@+llvm_extension+llvm-project.marker`: the first `DIRENTS` lines
    /// and every other line, verbatim.
    const LLVM_PROJECT: &str = "9abdb130f2b331f38817ce0a3351c176239296c19c27c8c1d5f2bfe1766aa3b6
DIRENTS:@@+third_party_ext+llvm-raw// f1a3a156096ab9374112b181691556cb4387f2daba14ee791cdf1c02d2d33813
DIRENTS:@@+third_party_ext+llvm-raw//bolt 561b13e7b51fbc9c90093377f9fe1c573ff4e320888a11c58aebb675dc2f624f
FILE:@@+third_party_ext+llvm-raw//cmake/Modules/LLVMVersion.cmake b5ea015164d3e4093be2444c1d5b535f343a7c4ec83dfd365abecb2d8188aa39
FILE:@@+third_party_ext+llvm-raw//llvm/CMakeLists.txt c1e3ca8dd9d50801dd28b4a0dda6d798d84dd03f73545d6098148215baa1eae2
REPO_MAPPING:+third_party_ext+llvm-raw,llvm-raw +third_party_ext+llvm-raw
";

    /// envoy, WORKSPACE era, `@com_envoyproxy_protoc_gen_validate.marker`,
    /// verbatim: the #28 shape.
    const PGV: &str = "ac8e4e40febb7f744c1622ed499a462b1bf94bf69e1134e322953f77064b6817
FILE:@@//bazel/pgv.patch 39e2547a67d331214ed2e110689bb32f7d820d34b547f30957f1d4cf259dd902
";

    /// envoy, `@envoy_repo.marker`, the first lines: a repository rule that
    /// reads plain main-repository files.
    const ENVOY_REPO: &str = "2a3dc47272ac2d79aeacd6c8e7d450a59fe2abc838ca1277ea075c0df91a653f
FILE:@@//.github/config.yml 24319a2a0ad0e48f36724ab3283f5d3d973f64f0618c2c5a40ef2b253f792980
FILE:@@//API_VERSION.txt 2985be8b28d3ade858e8d8fb4bc22f565b1bf6020dff982dce141f7721b9999c
FILE:@@//VERSION.txt 214aaa250e63b99aefe8e1372b5d37e94bdb224ec8b64d069e6d05007b3b812c
";

    /// envoy, `@python3_12_host.marker`, verbatim: a `DIRENTS` of another
    /// repository's root, and environment lines with the null value.
    const PY_HOST: &str = "fd924a55915dbd864baf13f3fa1d2a8db58f7f3b38c3bc41fe30904b1b7d705e
DIRENTS:@@python3_12_x86_64-apple-darwin// 622878d050388f5d35b626f6e0f16ac8bc80b1475457d18adae846a0dd3585d6
ENV:RULES_PYTHON_REPO_DEBUG \\0
ENV:RULES_PYTHON_REPO_DEBUG_VERBOSITY \\0
";

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn markers(v: &[(&str, &str)]) -> Markers {
        let mut m = Markers::default();
        for (repo, text) in v {
            m.by_repo
                .insert(repo.to_string(), parse_marker(text).expect(repo));
        }
        m
    }

    fn names(d: &[DirtyRepository]) -> Vec<&str> {
        d.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn a_real_marker_parses_into_its_path_inputs() {
        let m = parse_marker(LLVM_RAW).unwrap();
        assert_eq!(m.inputs.len(), 6);
        assert_eq!(
            m.inputs[0],
            Input {
                kind: Kind::File,
                place: Place::Repo {
                    name: String::new(),
                    path: "third_party/llvm/build.patch".into()
                }
            },
            "`@@//` is the main repository, and the path has no colon"
        );

        let p = parse_marker(LLVM_PROJECT).unwrap();
        assert_eq!(p.other, 1, "REPO_MAPPING is read and set aside");
        assert_eq!(p.inputs.len(), 4);
        assert_eq!(
            p.inputs[0].place,
            Place::Repo {
                name: "+third_party_ext+llvm-raw".into(),
                path: String::new()
            },
            "a repository's root directory"
        );
        assert_eq!(p.inputs[0].kind, Kind::Dirents);

        let py = parse_marker(PY_HOST).unwrap();
        assert_eq!(py.other, 2, "ENV lines, with the null value `\\0`");
        assert_eq!(py.inputs.len(), 1);
    }

    #[test]
    fn every_spelling_bazel_parses_is_read() {
        assert_eq!(
            parse_place("@@//a/b.patch"),
            Some(Place::Repo {
                name: "".into(),
                path: "a/b.patch".into()
            })
        );
        assert_eq!(
            parse_place("//a/b.patch"),
            Some(Place::Repo {
                name: "".into(),
                path: "a/b.patch".into()
            }),
            "no `@`"
        );
        assert_eq!(
            parse_place("@foo//x"),
            Some(Place::Repo {
                name: "foo".into(),
                path: "x".into()
            }),
            "one `@`: Bazel parses the name as canonical all the same"
        );
        assert_eq!(
            parse_place("@@rules_ml_toolchain+//cc/x.bzl"),
            Some(Place::Repo {
                name: "rules_ml_toolchain+".into(),
                path: "cc/x.bzl".into()
            })
        );
        assert_eq!(
            parse_place("/usr/include/stdio.h"),
            Some(Place::Absolute("/usr/include/stdio.h".into()))
        );
        assert_eq!(parse_place("relative/path"), None);
    }

    #[test]
    fn escapes_are_undone_and_a_dirtree_line_parses() {
        assert_eq!(unescape("a\\sb\\\\c\\nd"), "a b\\c\nd");
        let m = parse_marker("key\nDIRTREE:@@//vendor/my\\sdir 1234\n").unwrap();
        assert_eq!(
            m.inputs,
            vec![Input {
                kind: Kind::DirTree,
                place: Place::Repo {
                    name: "".into(),
                    path: "vendor/my dir".into()
                }
            }]
        );
    }

    #[test]
    fn an_unknown_kind_makes_the_marker_unreadable_as_it_does_for_bazel() {
        assert!(parse_marker("key\nWHATEVER:x 1\n").is_err());
        assert!(parse_marker("").is_err(), "no rule key");
        assert!(parse_marker("key\n").unwrap().inputs.is_empty());
    }

    #[test]
    fn a_changed_patch_dirties_its_repository_and_the_repository_generated_from_it() {
        let m = markers(&[
            ("+third_party_ext+llvm-raw", LLVM_RAW),
            ("+llvm_extension+llvm-project", LLVM_PROJECT),
            ("envoy_repo", ENVOY_REPO),
        ]);
        let changed = set(&["third_party/llvm/build.patch"]);
        let d = dirty(
            &m,
            &Changes {
                changed: &changed,
                added_or_deleted: None,
            },
            Path::new("/nonexistent"),
        );
        assert_eq!(
            names(&d),
            vec!["+llvm_extension+llvm-project", "+third_party_ext+llvm-raw"]
        );
        assert_eq!(d[1].via, vec!["third_party/llvm/build.patch"]);
        assert_eq!(
            d[0].via,
            vec!["@@+third_party_ext+llvm-raw"],
            "through the chain, not a path"
        );

        let unrelated = set(&["xla/service/foo.cc"]);
        assert!(dirty(
            &m,
            &Changes {
                changed: &unrelated,
                added_or_deleted: None
            },
            Path::new("/nonexistent"),
        )
        .is_empty());
    }

    #[test]
    fn the_workspace_era_pgv_patch_dirties_its_repository() {
        let m = markers(&[
            ("com_envoyproxy_protoc_gen_validate", PGV),
            ("envoy_repo", ENVOY_REPO),
        ]);
        let changed = set(&["bazel/pgv.patch", "VERSION.txt"]);
        let d = dirty(
            &m,
            &Changes {
                changed: &changed,
                added_or_deleted: None,
            },
            Path::new("/nonexistent"),
        );
        assert_eq!(
            names(&d),
            vec!["com_envoyproxy_protoc_gen_validate", "envoy_repo"]
        );
    }

    #[test]
    fn dirents_match_only_an_entry_added_or_removed_directly_in_the_directory() {
        let m = markers(&[("r", "key\nDIRENTS:@@//vendor 1\n")]);
        let edit = set(&["vendor/a.c"]);
        let none = BTreeSet::new();
        let ws = Path::new("/nonexistent");
        assert!(
            dirty(
                &m,
                &Changes {
                    changed: &edit,
                    added_or_deleted: Some(&none)
                },
                ws
            )
            .is_empty(),
            "a content edit does not change a listing"
        );
        assert_eq!(
            dirty(
                &m,
                &Changes {
                    changed: &edit,
                    added_or_deleted: Some(&edit)
                },
                ws
            )
            .len(),
            1,
            "an added file does"
        );
        assert_eq!(
            dirty(
                &m,
                &Changes {
                    changed: &edit,
                    added_or_deleted: None
                },
                ws
            )
            .len(),
            1,
            "unknown: any changed child"
        );
        let deeper = set(&["vendor/sub/a.c"]);
        assert!(
            dirty(
                &m,
                &Changes {
                    changed: &deeper,
                    added_or_deleted: Some(&deeper)
                },
                ws
            )
            .is_empty(),
            "a listing is one level"
        );
    }

    #[test]
    fn dirtree_matches_by_prefix_and_not_by_a_sibling_with_the_same_prefix() {
        let m = markers(&[("r", "key\nDIRTREE:@@//vendor/lib 1\n")]);
        let ws = Path::new("/nonexistent");
        let under = set(&["vendor/lib/deep/x.h"]);
        let sibling = set(&["vendor/library/x.h"]);
        assert_eq!(
            dirty(
                &m,
                &Changes {
                    changed: &under,
                    added_or_deleted: None
                },
                ws
            )
            .len(),
            1
        );
        assert!(dirty(
            &m,
            &Changes {
                changed: &sibling,
                added_or_deleted: None
            },
            ws
        )
        .is_empty());
    }

    #[test]
    fn an_in_tree_repository_translates_a_changed_path_into_its_own_spelling() {
        let mut m = markers(&[("consumer", "key\nFILE:@@envoy_api+//bazel/x.bzl 1\n")]);
        m.in_tree.insert("envoy_api+".into(), "api".into());
        let changed = set(&["api/bazel/x.bzl"]);
        let d = dirty(
            &m,
            &Changes {
                changed: &changed,
                added_or_deleted: None,
            },
            Path::new("/nonexistent"),
        );
        assert_eq!(names(&d), vec!["consumer"]);
        assert_eq!(
            d[0].via,
            vec!["api/bazel/x.bzl"],
            "reported as the workspace path"
        );
    }

    #[test]
    fn an_absolute_path_inside_the_checkout_is_the_main_repository_path() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("tp")).unwrap();
        std::fs::write(ws.path().join("tp/a.patch"), "x").unwrap();
        let abs = std::fs::canonicalize(ws.path()).unwrap().join("tp/a.patch");
        let m = markers(&[("r", &format!("key\nFILE:{} 1\n", abs.display()))]);
        let changed = set(&["tp/a.patch"]);
        assert_eq!(
            dirty(
                &m,
                &Changes {
                    changed: &changed,
                    added_or_deleted: None
                },
                ws.path()
            )
            .len(),
            1
        );
    }

    #[test]
    fn markers_and_in_tree_links_are_read_from_the_output_base() {
        let ob = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("api")).unwrap();
        let ext = ob.path().join("external");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(ext.join("@+third_party_ext+llvm-raw.marker"), LLVM_RAW).unwrap();
        std::fs::write(ext.join("@broken.marker"), "key\nNOPE:x 1\n").unwrap();
        std::fs::create_dir_all(ext.join("+third_party_ext+llvm-raw")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(ws.path().join("api"), ext.join("envoy_api+")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/nonexistent/dangling", ext.join("stale")).unwrap();

        let m = Markers::read(ob.path(), ws.path());
        assert_eq!(
            m.by_repo.keys().collect::<Vec<_>>(),
            vec!["+third_party_ext+llvm-raw"]
        );
        assert_eq!(m.unreadable, set(&["broken"]));
        #[cfg(unix)]
        assert_eq!(
            m.in_tree.iter().collect::<Vec<_>>(),
            vec![(&"envoy_api+".to_string(), &"api".to_string())],
            "a dangling link is not followed"
        );

        let empty = Markers::read(&ob.path().join("nope"), ws.path());
        assert!(
            empty.by_repo.is_empty(),
            "no external/ is no markers, not an error"
        );
    }

    #[test]
    fn a_union_keeps_the_inputs_either_read_recorded() {
        let a = markers(&[("r", "key\nFILE:@@//old.patch 1\n")]);
        let b = markers(&[("r", "key2\nFILE:@@//new.patch 1\n"), ("s", PGV)]);
        let u = a.union(b);
        assert_eq!(u.by_repo["r"].inputs.len(), 2);
        assert!(u.by_repo.contains_key("s"));
    }

    #[test]
    fn a_label_is_matched_to_its_repository_in_either_spelling() {
        assert!(label_in_repo(
            "@llvm-project//llvm:Support",
            "+llvm_extension+llvm-project"
        ));
        assert!(label_in_repo(
            "@@+llvm_extension+llvm-project//llvm:Support",
            "+llvm_extension+llvm-project"
        ));
        assert!(label_in_repo("@abseil-cpp//absl:base", "abseil-cpp+"));
        assert!(label_in_repo("@com_github_x//:y", "com_github_x"));
        assert!(!label_in_repo(
            "@llvm-raw//:x",
            "+llvm_extension+llvm-project"
        ));
        assert!(!label_in_repo(
            "//llvm:Support",
            "+llvm_extension+llvm-project"
        ));
    }

    #[test]
    fn the_root_is_every_target_of_the_repository() {
        assert_eq!(
            root_pattern("+llvm_extension+llvm-project"),
            "@@+llvm_extension+llvm-project//...:*"
        );
    }
}
