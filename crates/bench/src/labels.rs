//! The label sets a change-set comparison actually compared, kept on disk.
//!
//! `changes-<pin>.json` records per-class *counts* and a capped handful of
//! example labels, and that cap is a schema commitment: the file is read by
//! the report and by published tooling, so it cannot grow to hold thousands
//! of labels. But the counts are also where an investigation starts and
//! stops — a change set where the tools named 990 targets cambium lacked was
//! only ever visible through twenty of them, and a regression that put 165
//! labels in `unclassified` could not be looked at offline at all, because
//! the sets themselves were gone the moment the run moved on.
//!
//! So the sets are written beside the JSON instead of into it, one directory
//! per change set:
//!
//! ```text
//! <results>/<subject>/changes/labels/<base>..<head>/
//!     cambium.txt.zst        what cambium reported
//!     <tool>.txt.zst         what each tool reported
//!     oracle.txt.zst         what the rdeps oracle reached, when it ran
//!     classes.tsv.zst        tool \t side \t class \t label, every disagreement
//! ```
//!
//! One label per line, sorted, zstd-compressed — tens of kilobytes a change
//! set, which is why none of it is behind a flag: a debugging artifact that
//! has to be asked for in advance is not there on the run that needed it.
//! [`crate::compare::driver`] writes them; `cambi bench labels` reads them
//! back.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::classify::{Class, Side};

/// The directory under `changes/` that holds every change set's sets.
pub const DIR: &str = "labels";

/// The file names that are not a tool's.
pub const CAMBIUM: &str = "cambium";
pub const ORACLE: &str = "oracle";

const SET_EXT: &str = "txt.zst";
const CLASSES_FILE: &str = "classes.tsv.zst";

/// Fast, not small. These files are read by a human running one command, not
/// shipped anywhere, and the level that matters for the run is the one that
/// does not show up next to a Bazel query.
const LEVEL: i32 = 3;

/// `<changes>/labels/<base>..<head>/`, as named on disk.
pub fn change_set_dir(changes_dir: &Path, base: &str, head: &str) -> PathBuf {
    changes_dir.join(DIR).join(format!("{base}..{head}"))
}

/// One classified disagreement: every label the classifier saw, not the
/// examples that survived the cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassRow {
    /// The tool cambium was being compared against when this label was
    /// classified. A label can disagree with one tool and not another, so the
    /// tool is part of the row rather than of the file name.
    pub tool: String,
    pub side: Side,
    pub class: Class,
    pub label: String,
}

/// Writes one sorted, compressed label set as `<name>.txt.zst`.
pub fn write_set<'a>(
    dir: &Path,
    name: &str,
    labels: impl IntoIterator<Item = &'a String>,
) -> Result<PathBuf> {
    let sorted: BTreeSet<&str> = labels.into_iter().map(String::as_str).collect();
    let mut body = String::new();
    for label in sorted {
        body.push_str(label);
        body.push('\n');
    }
    let path = dir.join(format!("{name}.{SET_EXT}"));
    write_compressed(&path, body.as_bytes())?;
    Ok(path)
}

/// Reads a set written by [`write_set`].
pub fn read_set(path: &Path) -> Result<BTreeSet<String>> {
    Ok(read_compressed(path)?
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Writes every classification of this change set as `classes.tsv.zst`.
pub fn write_classes(dir: &Path, rows: &[ClassRow]) -> Result<PathBuf> {
    // Sorted here rather than at the call site: the rows arrive tool by tool
    // in whatever order the difference walked, and a file that reorders itself
    // between two runs of the same change set cannot be diffed.
    let mut sorted: Vec<&ClassRow> = rows.iter().collect();
    sorted.sort_by(|a, b| {
        (&a.tool, a.class, side_name(a.side), &a.label).cmp(&(
            &b.tool,
            b.class,
            side_name(b.side),
            &b.label,
        ))
    });
    let mut body = String::new();
    for r in sorted {
        body.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            r.tool,
            side_name(r.side),
            r.class.name(),
            r.label
        ));
    }
    let path = dir.join(CLASSES_FILE);
    write_compressed(&path, body.as_bytes())?;
    Ok(path)
}

/// Reads the rows written by [`write_classes`].
///
/// A line this build cannot parse — an unknown class name from a newer
/// cambium, a truncated write — is skipped rather than raised: the file is
/// evidence about a run, and refusing to show any of it because one row is
/// unreadable is the wrong trade for a debugging tool.
pub fn read_classes(path: &Path) -> Result<Vec<ClassRow>> {
    let text = read_compressed(path)?;
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        let mut f = line.splitn(4, '\t');
        let (Some(tool), Some(side), Some(class), Some(label)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        let (Some(side), Some(class)) = (parse_side(side), parse_class(class)) else {
            continue;
        };
        out.push(ClassRow {
            tool: tool.to_string(),
            side,
            class,
            label: label.to_string(),
        });
    }
    Ok(out)
}

/// The stored sets in one change set's directory, by name (`cambium`,
/// `oracle`, each tool), sorted.
pub fn stored_sets(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for e in entries {
        let path = e?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(stem) = name.strip_suffix(&format!(".{SET_EXT}")) {
            out.push((stem.to_string(), path.clone()));
        }
    }
    out.sort();
    Ok(out)
}

/// The `classes.tsv.zst` of a change set directory, when it has one.
///
/// A globally invalidated change set has no classification — cambium answered
/// `//...` and nothing was compared — so the absence is an answer, not an
/// error.
pub fn classes_path(dir: &Path) -> Option<PathBuf> {
    let p = dir.join(CLASSES_FILE);
    p.exists().then_some(p)
}

/// Finds the stored change set `spec` (`base..head`) names.
///
/// Revisions are matched the way `bench replay --change-set` matches them:
/// either side may be an abbreviation of what was recorded, or the recorded
/// one an abbreviation of what was typed, so the sha a person copied out of a
/// log resolves whatever length it happens to be.
pub fn resolve(changes_dir: &Path, spec: &str) -> Result<PathBuf> {
    let (base, head) = spec
        .split_once("..")
        .context("--change-set takes base..head")?;
    let root = changes_dir.join(DIR);
    let mut hits: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(&root).with_context(|| {
        format!(
            "no stored label sets under {}; they are written by `cambi bench changes`",
            root.display()
        )
    })?;
    for e in entries {
        let path = e?.path();
        let Some((b, h)) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split_once(".."))
        else {
            continue;
        };
        if pre(b, base) && pre(h, head) {
            hits.push(path);
        }
    }
    hits.sort();
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => anyhow::bail!("no stored label sets for {spec} under {}", root.display()),
        // Two matches means the abbreviation was the ambiguous thing, and
        // picking one of them would answer a question about the wrong change
        // set.
        _ => anyhow::bail!(
            "{spec} names {} stored change sets; give more of the revisions",
            hits.len()
        ),
    }
}

fn pre(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
}

/// Which rows a `bench labels` invocation asked for.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub tool: Option<String>,
    pub class: Option<Class>,
    pub side: Option<Side>,
}

impl Filter {
    pub fn matches(&self, row: &ClassRow) -> bool {
        self.tool.as_ref().map_or(true, |t| *t == row.tool)
            && self.class.map_or(true, |c| c == row.class)
            && self.side.map_or(true, |s| s == row.side)
    }

    /// Whether anything was asked for at all. Nothing means the person wants
    /// the stored sets, not the classification.
    pub fn is_empty(&self) -> bool {
        self.tool.is_none() && self.class.is_none() && self.side.is_none()
    }

    pub fn apply(&self, rows: &[ClassRow]) -> Vec<ClassRow> {
        rows.iter().filter(|r| self.matches(r)).cloned().collect()
    }
}

/// The three-way shape of one tool's disagreement with cambium.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetDiff {
    pub tool: String,
    pub only_tool: usize,
    pub only_cambium: usize,
    pub both: usize,
}

/// Compares each stored tool set against the stored cambium set.
///
/// The oracle is not a tool and is left out: it answers a different question
/// (what Bazel's own `rdeps` reached) and a "only in the oracle" count would
/// read as a disagreement between two tools when it is a recall check.
pub fn diff(dir: &Path) -> Result<Vec<SetDiff>> {
    let sets = stored_sets(dir)?;
    let cambium = match sets.iter().find(|(n, _)| n == CAMBIUM) {
        Some((_, p)) => read_set(p)?,
        None => anyhow::bail!("{} has no {CAMBIUM}.{SET_EXT}", dir.display()),
    };
    let mut out = Vec::new();
    for (name, path) in &sets {
        if name == CAMBIUM || name == ORACLE {
            continue;
        }
        let tool = read_set(path)?;
        out.push(SetDiff {
            tool: name.clone(),
            only_tool: tool.difference(&cambium).count(),
            only_cambium: cambium.difference(&tool).count(),
            both: tool.intersection(&cambium).count(),
        });
    }
    Ok(out)
}

pub fn side_name(side: Side) -> &'static str {
    match side {
        Side::Cambium => "cambium",
        Side::Tool => "tool",
    }
}

pub fn parse_side(s: &str) -> Option<Side> {
    match s {
        "cambium" => Some(Side::Cambium),
        "tool" => Some(Side::Tool),
        _ => None,
    }
}

pub fn parse_class(s: &str) -> Option<Class> {
    Class::ALL.into_iter().find(|c| c.name() == s)
}

fn write_compressed(path: &Path, raw: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let file =
        std::fs::File::create(path).with_context(|| format!("writing {}", path.display()))?;
    let mut enc = zstd::Encoder::new(file, LEVEL)?;
    // The same reasoning as the index format's: a compressed stream turns one
    // flipped bit into garbage from there on, and zstd's own checksum turns
    // that into a decode error instead.
    enc.include_checksum(true)?;
    enc.write_all(raw)?;
    enc.finish()
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn read_compressed(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let raw = zstd::decode_all(&bytes[..])
        .with_context(|| format!("{} will not decompress", path.display()))?;
    String::from_utf8(raw).with_context(|| format!("{} is not text", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn rows() -> Vec<ClassRow> {
        vec![
            ClassRow {
                tool: "bazel-diff".into(),
                side: Side::Cambium,
                class: Class::CambiumExtraBzl,
                label: "//a:a".into(),
            },
            ClassRow {
                tool: "bazel-diff".into(),
                side: Side::Tool,
                class: Class::CambiumMissing,
                label: "//b:b".into(),
            },
            ClassRow {
                tool: "tango".into(),
                side: Side::Cambium,
                class: Class::Unclassified,
                label: "//c:c".into(),
            },
        ]
    }

    #[test]
    fn a_label_set_survives_the_round_trip_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_set(
            dir.path(),
            "bazel-diff",
            &labels(&["//z:z", "//a:a", "//m:m"]),
        )
        .unwrap();
        assert!(path.ends_with("bazel-diff.txt.zst"));
        let back = read_set(&path).unwrap();
        assert_eq!(
            back.into_iter().collect::<Vec<_>>(),
            labels(&["//a:a", "//m:m", "//z:z"])
        );
    }

    #[test]
    fn an_empty_set_is_a_file_and_not_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_set(dir.path(), CAMBIUM, &labels(&[])).unwrap();
        assert!(path.exists(), "the answer 'nothing' has to be recorded too");
        assert!(read_set(&path).unwrap().is_empty());
    }

    #[test]
    fn every_classification_survives_the_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_classes(dir.path(), &rows()).unwrap();
        let mut back = read_classes(&path).unwrap();
        back.sort_by(|a, b| a.label.cmp(&b.label));
        let mut want = rows();
        want.sort_by(|a, b| a.label.cmp(&b.label));
        assert_eq!(back, want);
    }

    #[test]
    fn an_unreadable_row_does_not_hide_the_readable_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLASSES_FILE);
        write_compressed(
            &path,
            b"bazel-diff\ttool\tcambium_missing\t//b:b\nbazel-diff\ttool\tfrom_the_future\t//x:x\nnonsense\n",
        )
        .unwrap();
        let back = read_classes(&path).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].label, "//b:b");
    }

    #[test]
    fn a_filter_narrows_by_tool_class_and_side() {
        let rows = rows();
        assert_eq!(Filter::default().apply(&rows).len(), 3);
        assert!(Filter::default().is_empty());
        let by_tool = Filter {
            tool: Some("bazel-diff".into()),
            ..Filter::default()
        };
        assert_eq!(by_tool.apply(&rows).len(), 2);
        assert!(!by_tool.is_empty());
        let failures = Filter {
            class: Some(Class::Unclassified),
            ..Filter::default()
        };
        assert_eq!(failures.apply(&rows)[0].label, "//c:c");
        let both = Filter {
            tool: Some("bazel-diff".into()),
            side: Some(Side::Tool),
            class: None,
        };
        assert_eq!(both.apply(&rows)[0].label, "//b:b");
    }

    #[test]
    fn the_diff_counts_the_three_ways_a_label_can_land() {
        let dir = tempfile::tempdir().unwrap();
        write_set(dir.path(), CAMBIUM, &labels(&["//a:a", "//b:b"])).unwrap();
        write_set(dir.path(), "bazel-diff", &labels(&["//b:b", "//c:c"])).unwrap();
        // Present and deliberately ignored: the oracle answers a different
        // question from a tool's.
        write_set(dir.path(), ORACLE, &labels(&["//a:a"])).unwrap();
        let d = diff(dir.path()).unwrap();
        assert_eq!(
            d,
            vec![SetDiff {
                tool: "bazel-diff".into(),
                only_tool: 1,
                only_cambium: 1,
                both: 1,
            }]
        );
    }

    #[test]
    fn a_change_set_resolves_from_an_abbreviated_revision() {
        let dir = tempfile::tempdir().unwrap();
        let changes = dir.path();
        std::fs::create_dir_all(change_set_dir(changes, "aaaaaaaaaa", "bbbbbbbbbb")).unwrap();
        std::fs::create_dir_all(change_set_dir(changes, "cccccccccc", "dddddddddd")).unwrap();
        assert_eq!(
            resolve(changes, "aaaa..bbbb").unwrap(),
            change_set_dir(changes, "aaaaaaaaaa", "bbbbbbbbbb")
        );
        assert!(resolve(changes, "eeee..ffff").is_err());
    }

    #[test]
    fn a_prefix_that_names_two_change_sets_is_an_error_not_a_guess() {
        let dir = tempfile::tempdir().unwrap();
        let changes = dir.path();
        std::fs::create_dir_all(change_set_dir(changes, "aaaa1111", "bbbb1111")).unwrap();
        std::fs::create_dir_all(change_set_dir(changes, "aaaa2222", "bbbb2222")).unwrap();
        let err = resolve(changes, "aaaa..bbbb").unwrap_err().to_string();
        assert!(err.contains("2 stored change sets"), "{err}");
    }

    #[test]
    fn the_names_of_the_sets_are_the_files_that_are_there() {
        let dir = tempfile::tempdir().unwrap();
        write_set(dir.path(), CAMBIUM, &labels(&["//a:a"])).unwrap();
        write_set(dir.path(), "tango", &labels(&["//a:a"])).unwrap();
        write_classes(dir.path(), &rows()).unwrap();
        let names: Vec<String> = stored_sets(dir.path())
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec![CAMBIUM.to_string(), "tango".to_string()]);
        assert!(classes_path(dir.path()).is_some());
    }
}
