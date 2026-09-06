//! Persistent target-graph index for cambium.
//!
//! The index is a compressed sparse row graph over interned Bazel labels, plus
//! the per-package bookkeeping needed to replace one package's rows without
//! touching the rest of the graph. See the design spec, sections 5 and 6.
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

pub mod builder;
pub mod csr;
pub mod data;
pub mod format;
pub mod label;
pub mod overlay;
pub mod space;
pub mod strtab;
pub mod view;
pub mod walk;

pub use builder::{
    new_lineage, IndexBuilder, PackageDelta, PackageUpdate, SpliceReport, TargetRow,
};
pub use csr::{Csr, CsrRef};
pub use data::{
    flags, pkg_flags, Counts, Header, IndexData, IndexRef, NodeKind, Provenance, NO_ID,
};
pub use format::{
    commit_pointer_key, index_file_name, index_key, parse_commit_pointer_key, parse_index_key,
    temp_suffix, IndexError, IndexFile, OwnedIndexBytes, FORMAT_VERSION,
};
pub use overlay::{Overlay, OverlaySize};
pub use strtab::{Interner, StrTabRef};
pub use view::GraphView;
pub use walk::{walk, walk_deps, walk_edges, walk_view, WalkOptions, WalkResult};
