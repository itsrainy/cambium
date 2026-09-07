//! On-disk index format.
//!
//! ```text
//! [0  ..8 )  magic "CAMBIDX1"
//! [8  ..12)  format_version u32le
//! [12 ..16)  endian marker 0x04030201
//! [16 ..24)  payload offset u64le (always 64)
//! [24 ..32)  payload length u64le
//! [32 ..40)  trailer offset u64le
//! [40 ..48)  trailer length u64le
//! [48 ..64)  reserved, zero
//! [payload)  rkyv archive of IndexData — the graph, and nothing else
//! [trailer)  rkyv archive of Provenance — which commit, and when
//! ```
//!
//! The split is the point. The payload is a pure function of the repository's
//! content, so two commits with the same graph produce byte-identical payloads;
//! everything that varies per build — the revision, the clock — lives in the
//! trailer. That is what makes the content-addressed store layout described in
//! `docs/STORE.md` a later change rather than a format break.
//!
//! The raw prefix exists so that a truncated or foreign file is rejected before
//! anything reads the archive, and so `index inspect` can report the version of
//! a file it cannot otherwise parse. Load is O(1): mmap, check the prefix, cast.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use crate::data::{ArchivedIndexData, IndexData, IndexRef, Provenance};

pub const MAGIC: [u8; 8] = *b"CAMBIDX1";

/// Bumped to 2 when provenance moved out of the payload into the trailer, and
/// to 3 when provenance gained the partial-load fields.
pub const FORMAT_VERSION: u32 = 3;
pub const PREFIX_LEN: usize = 64;
const ENDIAN_MARKER: u32 = 0x0403_0201;
/// rkyv archives must start on this boundary for `access` to be sound.
const ALIGN: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not a cambium index (bad magic)")]
    BadMagic { path: PathBuf },
    #[error("{path} has format version {found}, this build understands {expected}")]
    BadVersion {
        path: PathBuf,
        found: u32,
        expected: u32,
    },
    #[error("{path} was written on a big-endian host")]
    BadEndian { path: PathBuf },
    #[error("{path} is truncated: wants {want} bytes, file has {have}")]
    Truncated { path: PathBuf, want: u64, have: u64 },
    #[error("serialising index: {0}")]
    Serialize(String),
}

type Result<T> = std::result::Result<T, IndexError>;

/// Store key for an index: `indexes/<lineage>/<tree>.bin`.
///
/// Lineage first because ids are only comparable within one, so a lineage is
/// the unit you list, garbage-collect, and retire. The second component is the
/// git *tree* hash of the revision the index describes, not the commit sha:
/// two commits with the same tree have the same graph, so a rebased or
/// squash-merged commit finds the index its pre-rebase twin published.
/// `commit_pointer_key` is the sha-addressed pointer for tooling that only has
/// a sha.
pub fn index_key(lineage: &[u8; 16], tree: &str) -> String {
    format!("indexes/{}/{}.bin", hex(lineage), tree)
}

/// Store key of the small pointer file `commits/<lineage>/<sha>`, whose
/// content is the tree hash the index for that commit is keyed under.
pub fn commit_pointer_key(lineage: &[u8; 16], commit: &str) -> String {
    format!("commits/{}/{}", hex(lineage), commit)
}

/// The lineage and commit a pointer key refers to, if it is one.
pub fn parse_commit_pointer_key(key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix("commits/")?;
    let (lineage, commit) = rest.split_once('/')?;
    (!lineage.is_empty() && !commit.is_empty() && !commit.contains('/'))
        .then(|| (lineage.to_string(), commit.to_string()))
}

/// The lineage and commit a store key refers to, if it is one.
pub fn parse_index_key(key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix("indexes/")?;
    let (lineage, file) = rest.split_once('/')?;
    let commit = file.strip_suffix(".bin")?;
    (!lineage.is_empty() && !commit.is_empty()).then(|| (lineage.to_string(), commit.to_string()))
}

/// Legacy flat name, still recognised when listing an older store.
pub fn index_file_name(lineage: &[u8; 16], commit: &str) -> String {
    format!("cambium-index-{}-{}.bin", hex(lineage), commit)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The payload bytes for `data`: the graph, with no provenance in them.
///
/// Deterministic — the same graph always produces the same bytes — which is
/// what makes republishing an index idempotent and what a content-addressed
/// store would key on.
pub fn payload_bytes(data: &IndexData) -> Result<Vec<u8>> {
    let payload = rkyv::to_bytes::<rkyv::rancor::Error>(data)
        .map_err(|e| IndexError::Serialize(e.to_string()))?;
    Ok(payload.to_vec())
}

/// Serialises `data` into the framed on-disk form.
pub fn to_bytes(data: &IndexData) -> Result<Vec<u8>> {
    let payload = payload_bytes(data)?;
    let trailer = rkyv::to_bytes::<rkyv::rancor::Error>(&data.provenance)
        .map_err(|e| IndexError::Serialize(e.to_string()))?;

    let payload_off = PREFIX_LEN;
    let trailer_off = align_up(payload_off + payload.len());
    let mut out = Vec::with_capacity(trailer_off + trailer.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&ENDIAN_MARKER.to_le_bytes());
    out.extend_from_slice(&(payload_off as u64).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&(trailer_off as u64).to_le_bytes());
    out.extend_from_slice(&(trailer.len() as u64).to_le_bytes());
    out.resize(PREFIX_LEN, 0);
    out.extend_from_slice(&payload);
    out.resize(trailer_off, 0);
    out.extend_from_slice(&trailer);
    Ok(out)
}

fn align_up(n: usize) -> usize {
    n.div_ceil(ALIGN) * ALIGN
}

/// A suffix unique to this call, for temp files that are about to be renamed
/// into place.
///
/// Process id alone is not enough: two threads in one process publishing the
/// same key would pick the same temp name, and one would rename the file out
/// from under the other. The counter makes each writer's temp file its own.
pub fn temp_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}.{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// Writes an index atomically: a temp file in the same directory, then a
/// rename.
///
/// Two writers publishing the same index concurrently is safe and expected —
/// the payload is deterministic, so they are writing the same bytes, and the
/// rename means a reader sees either the old file or a complete new one, never
/// a partial write. Each writer gets its own temp name, so they do not collide
/// with each other either.
pub fn write_index(path: &Path, data: &IndexData) -> Result<()> {
    let bytes = to_bytes(data)?;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|source| IndexError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
    }
    let tmp = path.with_extension(format!("bin.tmp.{}", temp_suffix()));
    {
        let mut f = File::create(&tmp).map_err(|source| IndexError::Io {
            path: tmp.clone(),
            source,
        })?;
        f.write_all(&bytes).map_err(|source| IndexError::Io {
            path: tmp.clone(),
            source,
        })?;
        f.sync_all().map_err(|source| IndexError::Io {
            path: tmp.clone(),
            source,
        })?;
    }
    std::fs::rename(&tmp, path).map_err(|source| IndexError::Io {
        path: path.to_path_buf(),
        source,
    })
}

struct Frame {
    payload: (usize, usize),
    trailer: (usize, usize),
}

fn check_prefix(path: &Path, bytes: &[u8], file_len: u64) -> Result<Frame> {
    if bytes.len() < PREFIX_LEN || bytes[..8] != MAGIC {
        return Err(IndexError::BadMagic {
            path: path.to_path_buf(),
        });
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if version != FORMAT_VERSION {
        return Err(IndexError::BadVersion {
            path: path.to_path_buf(),
            found: version,
            expected: FORMAT_VERSION,
        });
    }
    if u32::from_le_bytes(bytes[12..16].try_into().unwrap()) != ENDIAN_MARKER {
        return Err(IndexError::BadEndian {
            path: path.to_path_buf(),
        });
    }
    let read = |a: usize| u64::from_le_bytes(bytes[a..a + 8].try_into().unwrap());
    let (p_off, p_len, t_off, t_len) = (read(16), read(24), read(32), read(40));
    let want = (t_off + t_len).max(p_off + p_len);
    if want > file_len {
        return Err(IndexError::Truncated {
            path: path.to_path_buf(),
            want,
            have: file_len,
        });
    }
    Ok(Frame {
        payload: (p_off as usize, p_len as usize),
        trailer: (t_off as usize, t_len as usize),
    })
}

/// An mmapped index file. Holds the mapping; views borrow from it.
pub struct IndexFile {
    path: PathBuf,
    map: Mmap,
    frame: Frame,
    provenance: Provenance,
}

impl IndexFile {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| IndexError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let file_len = file
            .metadata()
            .map_err(|source| IndexError::Io {
                path: path.to_path_buf(),
                source,
            })?
            .len();
        // SAFETY: the mapping is read-only and the file is expected to be
        // immutable for the process lifetime (indexes are written atomically
        // and never edited in place).
        let map = unsafe { Mmap::map(&file) }.map_err(|source| IndexError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let frame = check_prefix(path, &map, file_len)?;
        let trailer = &map[frame.trailer.0..frame.trailer.0 + frame.trailer.1];
        // SAFETY: as `archived`; the trailer is a few dozen bytes written by us.
        let provenance = rkyv::deserialize::<Provenance, rkyv::rancor::Error>(unsafe {
            rkyv::access_unchecked::<crate::data::ArchivedProvenance>(trailer)
        })
        .map_err(|e| IndexError::Serialize(e.to_string()))?;
        Ok(Self {
            path: path.to_path_buf(),
            map,
            frame,
            provenance,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn payload(&self) -> &[u8] {
        &self.map[self.frame.payload.0..self.frame.payload.0 + self.frame.payload.1]
    }

    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    pub fn archived(&self) -> &ArchivedIndexData {
        // SAFETY: the prefix check established magic, version and length. The
        // archive itself is trusted: indexes come from our own baseline store,
        // and full bytecheck validation would defeat the O(1) load requirement.
        // `access_unchecked` needs the payload to be aligned to the archive's
        // alignment; the mapping starts page-aligned and PREFIX_LEN is 64.
        debug_assert_eq!(self.payload().as_ptr() as usize % ALIGN, 0);
        unsafe { rkyv::access_unchecked::<ArchivedIndexData>(self.payload()) }
    }

    pub fn view(&self) -> IndexRef<'_> {
        self.archived().view_with(&self.provenance)
    }

    pub fn deserialize(&self) -> Result<IndexData> {
        let mut data = rkyv::deserialize::<IndexData, rkyv::rancor::Error>(self.archived())
            .map_err(|e| IndexError::Serialize(e.to_string()))?;
        // Skipped on the way in, restored from the trailer on the way out.
        data.provenance = self.provenance.clone();
        Ok(data)
    }

    pub fn file_size(&self) -> usize {
        self.map.len()
    }
}

/// Loads an index from a byte buffer, for callers that already have the bytes.
pub struct OwnedIndexBytes {
    bytes: rkyv::util::AlignedVec<16>,
    provenance: Provenance,
}

impl OwnedIndexBytes {
    pub fn new(raw: &[u8]) -> Result<Self> {
        let frame = check_prefix(Path::new("<memory>"), raw, raw.len() as u64)?;
        let mut bytes = rkyv::util::AlignedVec::<16>::with_capacity(frame.payload.1);
        bytes.extend_from_slice(&raw[frame.payload.0..frame.payload.0 + frame.payload.1]);
        let mut trailer = rkyv::util::AlignedVec::<16>::with_capacity(frame.trailer.1);
        trailer.extend_from_slice(&raw[frame.trailer.0..frame.trailer.0 + frame.trailer.1]);
        let provenance = rkyv::deserialize::<Provenance, rkyv::rancor::Error>(unsafe {
            rkyv::access_unchecked::<crate::data::ArchivedProvenance>(&trailer)
        })
        .map_err(|e| IndexError::Serialize(e.to_string()))?;
        Ok(Self { bytes, provenance })
    }

    pub fn archived(&self) -> &ArchivedIndexData {
        // SAFETY: as `IndexFile::archived`, with alignment from AlignedVec.
        unsafe { rkyv::access_unchecked::<ArchivedIndexData>(&self.bytes) }
    }

    pub fn view(&self) -> IndexRef<'_> {
        self.archived().view_with(&self.provenance)
    }
}
