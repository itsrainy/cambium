//! On-disk index format.
//!
//! ```text
//! [0  ..8 )  magic "CAMBIDX1"
//! [8  ..12)  format_version u32le
//! [12 ..16)  endian marker 0x04030201
//! [16 ..24)  payload offset u64le (always 64)
//! [24 ..32)  payload length u64le
//! [32 ..64)  reserved, zero
//! [64 ..  )  rkyv archive of IndexData
//! ```
//!
//! The raw prefix exists so that a truncated or foreign file is rejected before
//! anything reads the archive, and so `index inspect` can report the version of
//! a file it cannot otherwise parse. Load is O(1): mmap, check the prefix, cast.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use crate::data::{ArchivedIndexData, IndexData, IndexRef};

pub const MAGIC: [u8; 8] = *b"CAMBIDX1";
pub const FORMAT_VERSION: u32 = 1;
pub const PREFIX_LEN: usize = 64;
const ENDIAN_MARKER: u32 = 0x0403_0201;

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
    #[error("{path} is truncated: payload wants {want} bytes, file has {have}")]
    Truncated {
        path: PathBuf,
        want: u64,
        have: u64,
    },
    #[error("serialising index: {0}")]
    Serialize(String),
}

type Result<T> = std::result::Result<T, IndexError>;

/// Canonical file name for an index: `cambium-index-<lineage>-<commit>.bin`.
pub fn index_file_name(lineage: &[u8; 16], commit: &str) -> String {
    let mut lin = String::with_capacity(32);
    for b in lineage {
        use std::fmt::Write as _;
        let _ = write!(lin, "{b:02x}");
    }
    format!("cambium-index-{lin}-{commit}.bin")
}

/// Serialises `data` into the framed on-disk form.
pub fn to_bytes(data: &IndexData) -> Result<Vec<u8>> {
    let payload = rkyv::to_bytes::<rkyv::rancor::Error>(data)
        .map_err(|e| IndexError::Serialize(e.to_string()))?;
    let mut out = Vec::with_capacity(PREFIX_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&ENDIAN_MARKER.to_le_bytes());
    out.extend_from_slice(&(PREFIX_LEN as u64).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.resize(PREFIX_LEN, 0);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Writes an index atomically (temp file + rename in the same directory).
pub fn write_index(path: &Path, data: &IndexData) -> Result<()> {
    let bytes = to_bytes(data)?;
    let tmp = path.with_extension("bin.tmp");
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

fn check_prefix(path: &Path, bytes: &[u8], file_len: u64) -> Result<u64> {
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
    let off = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let len = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    if off + len > file_len {
        return Err(IndexError::Truncated {
            path: path.to_path_buf(),
            want: off + len,
            have: file_len,
        });
    }
    Ok(len)
}

/// An mmapped index file. Holds the mapping; views borrow from it.
pub struct IndexFile {
    path: PathBuf,
    map: Mmap,
    payload_len: usize,
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
        let payload_len = check_prefix(path, &map, file_len)? as usize;
        Ok(Self {
            path: path.to_path_buf(),
            map,
            payload_len,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn payload(&self) -> &[u8] {
        &self.map[PREFIX_LEN..PREFIX_LEN + self.payload_len]
    }

    pub fn archived(&self) -> &ArchivedIndexData {
        // SAFETY: the prefix check established magic, version and length. The
        // archive itself is trusted: indexes come from our own baseline store,
        // and full bytecheck validation would defeat the O(1) load requirement.
        // `access_unchecked` needs the payload to be aligned to the archive's
        // alignment; the mapping starts page-aligned and PREFIX_LEN is 64.
        debug_assert_eq!(self.payload().as_ptr() as usize % 16, 0);
        unsafe { rkyv::access_unchecked::<ArchivedIndexData>(self.payload()) }
    }

    pub fn view(&self) -> IndexRef<'_> {
        self.archived().view()
    }

    pub fn deserialize(&self) -> Result<IndexData> {
        rkyv::deserialize::<IndexData, rkyv::rancor::Error>(self.archived())
            .map_err(|e| IndexError::Serialize(e.to_string()))
    }

    pub fn file_size(&self) -> usize {
        PREFIX_LEN + self.payload_len
    }
}

/// Loads an index from a byte buffer (used by tests and the HTTP store, which
/// already have the bytes in memory).
pub struct OwnedIndexBytes {
    bytes: rkyv::util::AlignedVec<16>,
}

impl OwnedIndexBytes {
    pub fn new(raw: &[u8]) -> Result<Self> {
        let len = check_prefix(Path::new("<memory>"), raw, raw.len() as u64)? as usize;
        let mut bytes = rkyv::util::AlignedVec::<16>::with_capacity(len);
        bytes.extend_from_slice(&raw[PREFIX_LEN..PREFIX_LEN + len]);
        Ok(Self { bytes })
    }

    pub fn archived(&self) -> &ArchivedIndexData {
        // SAFETY: as `IndexFile::archived`, with alignment guaranteed by AlignedVec.
        unsafe { rkyv::access_unchecked::<ArchivedIndexData>(&self.bytes) }
    }

    pub fn view(&self) -> IndexRef<'_> {
        self.archived().view()
    }
}
