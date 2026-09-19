//! Persistent target-graph index for cambium.
//!
//! The index is a compressed sparse row graph over interned Bazel labels, plus
//! the per-package bookkeeping needed to replace one package's rows without
//! touching the rest of the graph.
//!
//! ```no_run
//! use cambium_index::{IndexFile, walk::{walk, WalkOptions}};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let file = IndexFile::open(std::path::Path::new("index.bin"))?;
//! let view = file.view();
//! let direct = vec![0u32];
//! let result = walk(view, &direct, &WalkOptions::default());
//! for id in &result.affected {
//!     println!("{}", view.label(*id));
//! }
//! # Ok(()) }
//! ```

mod attribution;
pub mod builder;
pub mod csr;
pub mod data;
pub mod externals;
pub mod format;
pub mod label;
pub mod loads;
pub mod lockfile;
pub mod overlay;
pub mod sky;
pub mod space;
mod starlark;
pub mod strtab;
#[cfg(test)]
mod testutil;
mod util;
pub mod view;
mod vocabulary;
pub mod walk;
pub mod workspace;

pub use builder::{
    new_lineage, BzlClosure, IndexBuilder, PackageDelta, PackageUpdate, SpliceReport, TargetRow,
};
pub use csr::{Csr, CsrRef};
pub use data::{
    flags, pkg_flags, Counts, Header, IndexData, IndexRef, NodeKind, Provenance, NO_ID,
};
pub use externals::{
    build_repo_inputs, bzlmod_live, diff_module, dir_candidates, is_toolchain_module,
    read_live_module_facts,
    read_module_facts, read_module_facts_in, repo_setting_inputs, scan_module, DirCandidate,
    Mode, ModuleChange, ModuleFacts, PathMap, Pattern, RepoInputs, RepoSetting,
};
pub use format::{
    commit_pointer_key, index_file_name, index_key, is_zstd, parse_commit_pointer_key,
    parse_index_key, temp_suffix, Compression, IndexError, IndexFile, OwnedIndexBytes,
    CONTENT_REVISION, DEFAULT_ZSTD_LEVEL, FORMAT_VERSION, ZSTD_MAGIC,
};
pub use loads::{
    build_dirs_under, mark_traversed, repo_boundaries, Boundary, FileText, Load, LoadGraph, PackageClosure, RepoMapping,
};
pub use lockfile::diff_lock;
pub use overlay::{Overlay, OverlaySize};
pub use sky::{SkyLayer, SkyRef, SkyTables, SkyWalk};
pub use strtab::{Interner, StrTabRef};
pub use view::{labels_sorted, GraphView};
pub use walk::{walk, walk_deps, walk_edges, walk_view, WalkOptions, WalkResult};
pub use workspace::{diff_workspace, WorkspaceChange};
