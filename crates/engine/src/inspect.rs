//! Queries against a built index.
//!
//! These answer the questions an operator (or the service that will shell out
//! to this binary) asks of an index that already exists: what depends on this,
//! what does this depend on, how are the two connected, who consumes this file,
//! which package owns this path, what is in this package, and how big is the
//! thing.
//!
//! There is no graph logic here. Reachability is [`cambium_index::walk`] in one
//! direction or the other; ownership is [`cambium_classify::PackageSet`], the
//! same lookup the dirty set uses. A query that disagreed with a run would be
//! worse than no query at all.

use std::collections::BTreeMap;

use cambium_classify::PackageSet;
use cambium_index::{label, walk, IndexRef, NodeKind, WalkOptions};
use serde::{Deserialize, Serialize};

use crate::report::SCHEMA_VERSION;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("{0} is not in this index")]
    UnknownLabel(String),
    #[error("no package named {0} in this index")]
    UnknownPackage(String),
    #[error("{0} is not a node kind; try rule, source_file, generated_file, package_group, or tombstone")]
    UnknownKind(String),
}

type Result<T> = std::result::Result<T, QueryError>;

use crate::report::schema_version_of_a_document_without_one as schema_version;

/// Queries over one index.
#[derive(Clone, Copy)]
pub struct IndexQuery<'a> {
    view: IndexRef<'a>,
}

/// Shared options for the reachability queries.
#[derive(Clone, Debug, Default)]
pub struct QueryOptions {
    /// Follow edges transitively. Off means one hop.
    pub transitive: bool,
    /// Keep `manual`-tagged rules.
    pub include_manual: bool,
    /// Report only test targets.
    pub tests_only: bool,
}

/// A list of labels, with enough context to know what produced it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TargetsResult {
    #[serde(default = "schema_version")]
    pub schema: u32,
    pub query: String,
    pub subject: String,
    pub transitive: bool,
    pub count: usize,
    pub targets: Vec<String>,
}

/// A path between two targets through the dependency edges.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathResult {
    #[serde(default = "schema_version")]
    pub schema: u32,
    pub query: String,
    pub from: String,
    pub to: String,
    pub found: bool,
    /// `from` first, `to` last. Empty when `found` is false.
    pub path: Vec<String>,
    /// Set when there is no path but one exists the other way round, which is
    /// almost always the question the caller meant to ask.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reversed_hint: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileConsumers {
    pub path: String,
    /// Package that owns the path, if any.
    pub package: Option<String>,
    /// The label the path has as a target, if a rule references it.
    pub label: Option<String>,
    pub consumers: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsumersResult {
    #[serde(default = "schema_version")]
    pub schema: u32,
    pub query: String,
    pub transitive: bool,
    pub files: Vec<FileConsumers>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathOwner {
    pub path: String,
    /// Owning package, or absent when no BUILD file covers the path.
    pub package: Option<String>,
    /// The label the path would have as a source file in that package.
    pub label: Option<String>,
    /// Whether that label is actually in the index.
    pub in_index: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OwnerResult {
    #[serde(default = "schema_version")]
    pub schema: u32,
    pub query: String,
    pub paths: Vec<PathOwner>,
}

/// One node, as `index inspect --label` prints it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TargetDetail {
    pub label: String,
    pub id: u32,
    pub kind: String,
    pub package: Option<String>,
    pub rule_class: Option<String>,
    pub is_test: bool,
    pub manual: bool,
    pub tombstone: bool,
    pub rule_hash: String,
    pub deps: Vec<String>,
    pub rdeps: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackageTargets {
    #[serde(default = "schema_version")]
    pub schema: u32,
    pub query: String,
    pub package: String,
    pub count: usize,
    /// `(kind, label)` pairs, sorted by label.
    pub targets: Vec<PackageTarget>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackageTarget {
    pub label: String,
    pub kind: String,
}

/// Byte accounting and shape of an index.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexStats {
    #[serde(default = "schema_version")]
    pub schema: u32,
    pub query: String,
    pub commit: String,
    pub lineage: String,
    pub format_version: u32,
    pub counts: CountStats,
    pub bytes: ByteStats,
    pub in_degree: DegreeStats,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CountStats {
    pub nodes: u32,
    pub edges: u64,
    pub packages: u32,
    pub rules: u32,
    pub tests: u32,
    pub source_files: u32,
    pub generated_files: u32,
    pub package_groups: u32,
    pub unknown: u32,
    pub tombstones: u32,
    pub tombstoned_packages: u32,
}

/// Per-component byte sizes. Every field is the size of the arrays as they sit
/// in the file; `framing` is whatever the file has left over — the header, the
/// archive's own pointers and its alignment padding.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ByteStats {
    pub file_total: u64,
    pub label_table: u64,
    pub label_table_share: f64,
    /// Node ids ordered by label — the array `IndexRef::id_of` binary-searches.
    /// Part of what labels cost, and not free: 4 bytes per node.
    pub label_sorted: u64,
    /// `label_table` + `label_sorted` over the file. This is the number to
    /// compare against a threshold for "are labels worth compressing".
    pub label_total_share: f64,
    pub rule_class_table: u64,
    pub forward_offsets: u64,
    pub forward_edges: u64,
    pub reverse_offsets: u64,
    pub reverse_edges: u64,
    /// The `files` CSR of the design spec is a view over `reverse`, not a
    /// stored array: only rules hold forward edges, so every in-edge of a file
    /// node comes from a rule and the reverse row is already the consumer list.
    /// Reported as zero so the accounting is explicit rather than absent.
    pub files_csr: u64,
    pub rule_hashes: u64,
    pub node_columns: u64,
    pub package_table: u64,
    pub framing: i64,
    /// What labels would cost stored as `(package_id, relative_name)` instead
    /// of whole strings. See [`LabelPacking`].
    pub label_packing: LabelPacking,
}

/// A projection, not a measurement of anything that exists.
///
/// Every label is `//package:name` (or `@repo//package:name`), and the node
/// columns already carry each node's package id, so the `//package:` prefix is
/// stored twice: once in the package table, once at the head of every label.
/// Dropping it from the label table would cost nothing extra — the package id
/// is already there — so the saving is exactly the prefix bytes.
///
/// This is here so the "is it worth it" question can be answered on a real
/// repository rather than on the synthetic graphs, where labels are short and
/// the answer comes out lower than it would for deep source trees.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabelPacking {
    /// Label bytes that are the `//package:` prefix.
    pub prefix_bytes: u64,
    /// Nodes with no package id, whose labels would have to stay whole.
    pub unpackable_nodes: u32,
    /// `label_table` after packing.
    pub packed_label_table: u64,
    /// Fraction of the whole file that packing would remove.
    pub file_saving_share: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DegreeStats {
    pub max: u32,
    pub max_label: String,
    pub p99: u32,
    pub p50: u32,
    pub mean: f64,
}

impl<'a> IndexQuery<'a> {
    pub fn new(view: IndexRef<'a>) -> Self {
        Self { view }
    }

    pub fn view(&self) -> IndexRef<'a> {
        self.view
    }

    fn id_of(&self, label: &str) -> Result<u32> {
        self.view
            .labels
            .build_map()
            .get(label)
            .copied()
            .ok_or_else(|| QueryError::UnknownLabel(label.to_string()))
    }

    fn walk_opts(&self, o: &QueryOptions) -> WalkOptions {
        WalkOptions {
            include_manual: o.include_manual,
            explain: false,
        }
    }

    fn collect(&self, ids: &[u32]) -> Vec<String> {
        walk::labels_sorted(self.view, ids)
    }

    /// Targets that depend on `label`, directly or transitively.
    pub fn rdeps(&self, label: &str, o: &QueryOptions) -> Result<TargetsResult> {
        let id = self.id_of(label)?;
        let ids = if o.transitive {
            let r = walk::walk(self.view, &[id], &self.walk_opts(o));
            self.pick(&r, o)
        } else {
            self.filter_row(self.view.reverse.row(id), o)
        };
        Ok(TargetsResult {
            schema: SCHEMA_VERSION,
            query: "rdeps".into(),
            subject: label.to_string(),
            transitive: o.transitive,
            count: ids.len(),
            targets: self.collect(&ids),
        })
    }

    /// Targets `label` depends on, directly or transitively.
    pub fn deps(&self, label: &str, o: &QueryOptions) -> Result<TargetsResult> {
        let id = self.id_of(label)?;
        let ids = if o.transitive {
            let r = walk::walk_deps(self.view, &[id], &self.walk_opts(o));
            self.pick(&r, o)
        } else {
            self.filter_row(self.view.forward.row(id), o)
        };
        Ok(TargetsResult {
            schema: SCHEMA_VERSION,
            query: "deps".into(),
            subject: label.to_string(),
            transitive: o.transitive,
            count: ids.len(),
            targets: self.collect(&ids),
        })
    }

    fn pick(&self, r: &walk::WalkResult, o: &QueryOptions) -> Vec<u32> {
        if o.tests_only {
            r.tests.clone()
        } else {
            r.affected.clone()
        }
    }

    /// One hop, filtered the same way the transitive walk filters.
    fn filter_row(&self, row: &[u32], o: &QueryOptions) -> Vec<u32> {
        row.iter()
            .copied()
            .filter(|&id| {
                if self.view.is_tombstone(id) {
                    return false;
                }
                if !o.include_manual && self.view.is_manual(id) {
                    return false;
                }
                if o.tests_only {
                    return self.view.is_rule(id) && self.view.is_test[id as usize];
                }
                true
            })
            .collect()
    }

    /// The shortest dependency path from `from` to `to`.
    pub fn path(&self, from: &str, to: &str) -> Result<PathResult> {
        let from_id = self.id_of(from)?;
        let to_id = self.id_of(to)?;
        let opts = WalkOptions {
            include_manual: true,
            explain: true,
        };

        let forward = walk::walk_deps(self.view, &[from_id], &opts);
        if let Some(mut p) = forward.path_to_seed(to_id) {
            // path_to_seed walks parent pointers back to the seed, so it comes
            // out `to` first; the caller asked from -> to.
            p.reverse();
            return Ok(PathResult {
                schema: SCHEMA_VERSION,
                query: "path".into(),
                from: from.to_string(),
                to: to.to_string(),
                found: true,
                path: p.iter().map(|&i| self.view.label(i).to_string()).collect(),
                reversed_hint: None,
            });
        }

        // No path from -> to. If there is one the other way, say so: asking in
        // the wrong order is the common mistake, and the answer is right there.
        let back = walk::walk_deps(self.view, &[to_id], &opts);
        let hint = back.path_to_seed(from_id).map(|p| {
            format!(
                "no path from {from} to {to}, but {to} depends on {from} ({} hops); \
                 try swapping the arguments",
                p.len().saturating_sub(1)
            )
        });
        Ok(PathResult {
            schema: SCHEMA_VERSION,
            query: "path".into(),
            from: from.to_string(),
            to: to.to_string(),
            found: false,
            path: Vec::new(),
            reversed_hint: hint,
        })
    }

    /// The rules that consume each of these repo-relative file paths.
    pub fn consumers(&self, paths: &[String], o: &QueryOptions) -> Result<ConsumersResult> {
        let packages = PackageSet::from_index(self.view);
        let labels = self.view.labels.build_map();
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let package = packages.owner_of(path).map(|s| s.to_string());
            let label = package.as_ref().map(|pkg| label::source_label(pkg, path));
            let id = label.as_ref().and_then(|l| labels.get(l.as_str()).copied());
            let consumers = match id {
                Some(id) if o.transitive => {
                    let r = walk::walk(self.view, &[id], &self.walk_opts(o));
                    self.collect(&self.pick(&r, o))
                }
                Some(id) => {
                    let row = self.filter_row(self.view.file_consumers(id), o);
                    self.collect(&row)
                }
                None => Vec::new(),
            };
            files.push(FileConsumers {
                path: path.clone(),
                package,
                label: id.map(|i| self.view.label(i).to_string()),
                consumers,
            });
        }
        Ok(ConsumersResult {
            schema: SCHEMA_VERSION,
            query: "consumers".into(),
            transitive: o.transitive,
            files,
        })
    }

    /// Which package owns each path — the same longest-prefix lookup the dirty
    /// set uses to decide which package a changed file belongs to.
    pub fn owner(&self, paths: &[String]) -> Result<OwnerResult> {
        let packages = PackageSet::from_index(self.view);
        let labels = self.view.labels.build_map();
        let out = paths
            .iter()
            .map(|path| {
                let package = packages.owner_of(path).map(|s| s.to_string());
                let label = package.as_ref().map(|pkg| label::source_label(pkg, path));
                let in_index = label
                    .as_ref()
                    .map(|l| labels.contains_key(l.as_str()))
                    .unwrap_or(false);
                PathOwner {
                    path: path.clone(),
                    package,
                    label,
                    in_index,
                }
            })
            .collect();
        Ok(OwnerResult {
            schema: SCHEMA_VERSION,
            query: "owner".into(),
            paths: out,
        })
    }

    /// Everything a package owns, optionally of one kind.
    pub fn targets(&self, package: &str, kind: Option<NodeKind>) -> Result<PackageTargets> {
        let pkg = (0..self.view.num_packages())
            .find(|&i| self.view.package_name(i) == package)
            .ok_or_else(|| QueryError::UnknownPackage(package.to_string()))?;
        let mut targets: Vec<PackageTarget> = self
            .view
            .package_nodes(pkg)
            .iter()
            .copied()
            .filter(|&id| kind.map(|k| self.view.kind_of(id) == k).unwrap_or(true))
            .map(|id| PackageTarget {
                label: self.view.label(id).to_string(),
                kind: kind_name(self.view.kind_of(id)).to_string(),
            })
            .collect();
        targets.sort_by(|a, b| a.label.cmp(&b.label));
        Ok(PackageTargets {
            schema: SCHEMA_VERSION,
            query: "targets".into(),
            package: package.to_string(),
            count: targets.len(),
            targets,
        })
    }

    /// One node in full.
    pub fn detail(&self, label: &str) -> Result<TargetDetail> {
        let id = self.id_of(label)?;
        Ok(TargetDetail {
            label: label.to_string(),
            id,
            kind: kind_name(self.view.kind_of(id)).to_string(),
            package: self.view.package_of(id).map(|s| s.to_string()),
            rule_class: self.view.rule_class(id).map(|s| s.to_string()),
            is_test: self.view.is_test[id as usize],
            manual: self.view.is_manual(id),
            tombstone: self.view.is_tombstone(id),
            rule_hash: hex(&self.view.hash[id as usize]),
            deps: walk::labels_sorted(self.view, self.view.forward.row(id)),
            rdeps: walk::labels_sorted(self.view, self.view.reverse.row(id)),
        })
    }

    /// Byte accounting, counts, and in-degree distribution.
    ///
    /// `file_total` has to be supplied by the caller because it is a property
    /// of the file on disk, not of the view.
    pub fn stats(&self, file_total: u64) -> IndexStats {
        let v = self.view;
        let n = v.num_nodes() as usize;
        let np = v.num_packages() as usize;

        let mut counts = CountStats {
            nodes: v.num_nodes(),
            edges: v.num_edges() as u64,
            packages: v.num_packages(),
            rules: 0,
            tests: 0,
            source_files: 0,
            generated_files: 0,
            package_groups: 0,
            unknown: 0,
            tombstones: 0,
            tombstoned_packages: 0,
        };
        for id in 0..v.num_nodes() {
            match v.kind_of(id) {
                NodeKind::Rule => {
                    counts.rules += 1;
                    if v.is_test[id as usize] {
                        counts.tests += 1;
                    }
                }
                NodeKind::SourceFile => counts.source_files += 1,
                NodeKind::GeneratedFile => counts.generated_files += 1,
                NodeKind::PackageGroup | NodeKind::EnvironmentGroup => counts.package_groups += 1,
                NodeKind::Unknown => counts.unknown += 1,
                NodeKind::Tombstone => counts.tombstones += 1,
            }
        }
        for p in 0..v.num_packages() {
            if v.package_tombstoned(p) {
                counts.tombstoned_packages += 1;
            }
        }

        let label_table = (v.labels.bytes.len() + v.labels.offsets.len() * 4) as u64;
        let label_sorted = (v.label_sorted.len() * 4) as u64;
        let rule_class_table = (v.classes.bytes.len() + v.classes.offsets.len() * 4) as u64;
        let forward_offsets = (v.forward.offsets.len() * 4) as u64;
        let forward_edges = (v.forward.edges.len() * 4) as u64;
        let reverse_offsets = (v.reverse.offsets.len() * 4) as u64;
        let reverse_edges = (v.reverse.edges.len() * 4) as u64;
        let rule_hashes = (n * 32) as u64;
        // kind (1) + package (4) + class (4) + is_test (1) + flags (1)
        let node_columns = (n * 11) as u64;
        let package_table = (v.pkg_names.bytes.len()
            + v.pkg_names.offsets.len() * 4
            + v.pkg_nodes.offsets.len() * 4
            + v.pkg_nodes.edges.len() * 4
            + np * 32 * 2
            + np) as u64;
        // The prefix a packed label table would not have to store: for each
        // node whose package is known, the `//pkg:` (or `@repo//pkg:`) head of
        // its label. Offsets stay as they are, so the saving is prefix bytes.
        let mut prefix_bytes = 0u64;
        let mut unpackable_nodes = 0u32;
        for id in 0..v.num_nodes() {
            let label = v.labels.get(id);
            let pkg = v.package[id as usize];
            match (pkg != cambium_index::NO_ID, label.rfind(':')) {
                (true, Some(colon)) => prefix_bytes += colon as u64 + 1,
                _ => unpackable_nodes += 1,
            }
        }
        let label_packing = LabelPacking {
            prefix_bytes,
            unpackable_nodes,
            packed_label_table: label_table - prefix_bytes,
            file_saving_share: if file_total > 0 {
                prefix_bytes as f64 / file_total as f64
            } else {
                0.0
            },
        };

        let accounted = label_table
            + label_sorted
            + rule_class_table
            + forward_offsets
            + forward_edges
            + reverse_offsets
            + reverse_edges
            + rule_hashes
            + node_columns
            + package_table;

        let bytes = ByteStats {
            file_total,
            label_table,
            label_table_share: if file_total > 0 {
                label_table as f64 / file_total as f64
            } else {
                0.0
            },
            label_sorted,
            label_total_share: if file_total > 0 {
                (label_table + label_sorted) as f64 / file_total as f64
            } else {
                0.0
            },
            rule_class_table,
            forward_offsets,
            forward_edges,
            reverse_offsets,
            reverse_edges,
            files_csr: 0,
            rule_hashes,
            node_columns,
            package_table,
            framing: file_total as i64 - accounted as i64,
            label_packing,
        };

        let mut degrees: Vec<u32> = (0..v.num_nodes())
            .map(|id| v.reverse.row_len(id) as u32)
            .collect();
        let max_id = (0..v.num_nodes())
            .max_by_key(|&id| v.reverse.row_len(id))
            .unwrap_or(0);
        let sum: u64 = degrees.iter().map(|&d| d as u64).sum();
        degrees.sort_unstable();
        let at = |q: f64| -> u32 {
            if degrees.is_empty() {
                0
            } else {
                degrees[((degrees.len() as f64 * q) as usize).min(degrees.len() - 1)]
            }
        };
        let in_degree = DegreeStats {
            max: degrees.last().copied().unwrap_or(0),
            max_label: if v.num_nodes() > 0 {
                v.label(max_id).to_string()
            } else {
                String::new()
            },
            p99: at(0.99),
            p50: at(0.50),
            mean: if n > 0 { sum as f64 / n as f64 } else { 0.0 },
        };

        IndexStats {
            schema: SCHEMA_VERSION,
            query: "stats".into(),
            commit: v.commit.to_string(),
            lineage: hex(&v.lineage),
            format_version: v.format_version,
            counts,
            bytes,
            in_degree,
        }
    }

    /// Node counts by kind, for `inspect`.
    pub fn kind_histogram(&self) -> BTreeMap<&'static str, u32> {
        let mut h = BTreeMap::new();
        for id in 0..self.view.num_nodes() {
            *h.entry(kind_name(self.view.kind_of(id))).or_insert(0) += 1;
        }
        h
    }
}

pub fn kind_name(k: NodeKind) -> &'static str {
    match k {
        NodeKind::Rule => "rule",
        NodeKind::SourceFile => "source_file",
        NodeKind::GeneratedFile => "generated_file",
        NodeKind::PackageGroup => "package_group",
        NodeKind::EnvironmentGroup => "environment_group",
        NodeKind::Unknown => "unknown",
        NodeKind::Tombstone => "tombstone",
    }
}

pub fn parse_kind(s: &str) -> Result<NodeKind> {
    Ok(match s {
        "rule" => NodeKind::Rule,
        "source_file" | "source" => NodeKind::SourceFile,
        "generated_file" | "generated" => NodeKind::GeneratedFile,
        "package_group" => NodeKind::PackageGroup,
        "environment_group" => NodeKind::EnvironmentGroup,
        "unknown" => NodeKind::Unknown,
        "tombstone" => NodeKind::Tombstone,
        other => return Err(QueryError::UnknownKind(other.to_string())),
    })
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}
