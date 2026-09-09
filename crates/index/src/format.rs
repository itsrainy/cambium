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
//!
//! # Compression at rest
//!
//! A file on disk holds either the layout above verbatim or a single zstd
//! frame containing it, and [`IndexFile::open`] takes both: the first four
//! bytes are zstd's magic ([`ZSTD_MAGIC`]) or they are not, and anything that
//! is not goes down the path it always did. **The raw form is memory-mapped
//! and the parsed view borrows from the mapping; the compressed form is
//! decompressed once into an owned, 16-byte-aligned buffer and the view
//! borrows from that.** Either way nothing above this module copies the
//! payload, and everything above it sees the decompressed bytes only — the
//! header check, the store's tree keys and [`FORMAT_VERSION`] are all about
//! those, and compression is outside the format.
//!
//! Writers choose with [`Compression`]; [`write_index`] compresses at
//! [`DEFAULT_ZSTD_LEVEL`], which is what the store publish path and the local
//! cache get. See `docs/STORE.md` for the measured ratios.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use crate::data::{ArchivedIndexData, IndexData, IndexRef, Provenance};

pub const MAGIC: [u8; 8] = *b"CAMBIDX1";

/// Magic number that starts a zstd frame. A file beginning with these four
/// bytes is a compressed index; `MAGIC` cannot collide with it.
pub const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// The level [`Compression::default`] writes at. Chosen from the corpus
/// indexes: level 3 is 3.8-6.1x in well under a tenth of a second, level 9
/// buys about 7% more for twice the time, and level 19 buys about 30% more for
/// twenty to thirty times the time. Decompression costs the same ~60 ms at
/// every level, so the level only trades write latency against bytes.
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

/// How an index is stored on disk. Reading takes either form regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// The framed layout verbatim, as every index before this change.
    None,
    /// The framed layout inside one zstd frame, at this level.
    Zstd(i32),
}

impl Default for Compression {
    fn default() -> Self {
        Compression::Zstd(DEFAULT_ZSTD_LEVEL)
    }
}

/// Whether `bytes` begins a zstd frame, i.e. whether reading it means
/// decompressing first.
pub fn is_zstd(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes[..4] == ZSTD_MAGIC
}

/// Bumped to 2 when provenance moved out of the payload into the trailer, to
/// 3 when provenance gained the partial-load fields, and to 4 when the payload
/// gained the static `.bzl` load closure (`bzl_*` in
/// [`crate::data::IndexData`]) that replaced the `rbuildfiles` call, and to 5
/// when it gained the external-repository input map (`repo_*`, `bzl_repo_*`;
/// see [`crate::externals`]) that replaced whole-world invalidation on a
/// `MODULE.bazel` or repository-rule-input edit, and to 6 when it gained the
/// per-repository path mapping (`repo_map_*`) that tells an input-path edit
/// apart from an edit to a `.bzl` the repository exports, to 7 when the
/// closure gained the external labels each BUILD file loads *directly*
/// (`pkg_ext_*`), which the per-`.bzl` table had no row for (issue #4), and
/// to 8 when repository-rule inputs gained `repo_input_target_only`, which
/// tells a path the rule reads apart from a label it merely writes into a
/// generated BUILD file (issue #3).
///
/// TODO: at the next bump, drop `IndexData::pkg_bzl_hash` and the
/// `PackageUpdate::bzl_hash` that feeds it. Every writer fills the column and
/// no reader has consulted it since the static `.bzl` closure (format 4) began
/// answering "did this package's loads move?" from `bzl_consumers`; it is 32
/// bytes per package of write-only payload. Removing it moves the layout, so
/// it has to wait for a bump that is happening anyway rather than cause one.
///
/// Compression is outside this version. A file holds either the layout
/// verbatim or one zstd frame containing it (see the module docs); this
/// version, and every other header field, is of the *decompressed* bytes.
/// Turning compression on or off, or changing its level, is not a format
/// change and must not bump this.
pub const FORMAT_VERSION: u32 = 8;

/// What a correct build *derives* into an index, as against how the bytes are
/// laid out ([`FORMAT_VERSION`]) and what a rule hash means
/// (`cambium_proto::RULE_HASH_VERSION`).
///
/// An index is trusted on the strength of its `config_hash` alone: matching
/// hashes mean "the same cambium, configured the same way, would have written
/// this". A bug in the derivation breaks that promise without moving either of
/// the other two versions — the file is well-formed, its rule hashes are
/// comparable, and a column is simply missing — so the store keeps handing
/// every later run a base that a fixed cambium would never have written.
/// Bumping this is how such a fix retires the indexes written before it: the
/// engine mixes it into [`crate::config`-level](crate) hashing, an old blob
/// fails the compatibility check, and the next run rebuilds instead of
/// believing it.
///
/// 1 --- `BzlClosure::from_view` did not read `repo_map_*` back, so every
/// splice published an index with the per-repository path mappings dropped
/// (fixed in "externals: read the path mapping back out of the base index").
/// A run that fetched such a base treated every repository as unmapped and
/// re-queried ~1100 packages per change set.
///
/// 2 --- attribution of a repository's inputs became per-occurrence rather
/// than per-file ("externals: attribute a repository's inputs per call, not
/// per file"). An index written before that carries wider `patterns` and
/// `bzl_repos` rows, and path mappings with no `exhaustive` marker, so a run
/// that spliced onto one would keep re-querying the packages the change was
/// written to spare — a correct answer at the old cost, which is exactly what
/// this counter retires.
///
/// 3 --- a repository-rule label stopped claiming its whole package subtree as
/// a path prefix, and the pattern table gained a third kind of entry, the
/// label itself (issue #8; see [`crate::externals::Pattern`]). The layout is
/// unchanged — a label is told from a path by its leading `//` — but the
/// tables mean something new: an index written before this carries subtree
/// prefixes that dirty ~91% of a large repository's packages on a common
/// edit, and no label entries at all, so a run that spliced onto one would
/// keep both the over-broad answer and the whole-repository re-indexes it
/// causes.
pub const CONTENT_REVISION: u32 = 3;

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
    #[error("deserialising index at {path}: {reason}")]
    Deserialize { path: PathBuf, reason: String },
    #[error("{path} has a corrupt header: {reason}")]
    BadHeader { path: PathBuf, reason: String },
    #[error("{path} is a zstd frame that will not decompress: {reason}")]
    Decompress { path: PathBuf, reason: String },
    #[error("compressing index: {0}")]
    Compress(String),
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
    Ok(serialise(data)?.to_vec())
}

/// The payload archive, still in rkyv's aligned buffer.
///
/// [`to_bytes`] appends this straight into the framed output, so the payload —
/// the large part, hundreds of megabytes on a big repository — is copied once
/// rather than being flattened into a `Vec` first and copied again.
fn serialise(data: &IndexData) -> Result<rkyv::util::AlignedVec> {
    rkyv::to_bytes::<rkyv::rancor::Error>(data).map_err(|e| IndexError::Serialize(e.to_string()))
}

/// As [`serialise`], for the provenance trailer.
fn serialise_provenance(p: &Provenance) -> Result<rkyv::util::AlignedVec> {
    rkyv::to_bytes::<rkyv::rancor::Error>(p).map_err(|e| IndexError::Serialize(e.to_string()))
}

/// Serialises `data` into the on-disk form, compressed or not.
///
/// `Compression::None` is exactly [`to_bytes`]; `Zstd` wraps those same bytes
/// in one frame, so what a reader parses is identical either way.
pub fn to_bytes_with(data: &IndexData, compression: Compression) -> Result<Vec<u8>> {
    let raw = to_bytes(data)?;
    match compression {
        Compression::None => Ok(raw),
        Compression::Zstd(level) => compress(&raw, level),
    }
}

/// One zstd frame holding `raw`, with a content checksum.
///
/// The checksum is the point of doing this by hand rather than with
/// `encode_all`. The archive inside is cast in place and not validated — that
/// is what makes loading O(1) — so a flipped bit reaches `access_unchecked`,
/// and in a compressed stream one flipped bit corrupts everything after it
/// rather than one byte. zstd's own checksum turns that into a plain
/// [`IndexError::Decompress`] before any of it is parsed.
fn compress(raw: &[u8], level: i32) -> Result<Vec<u8>> {
    let fail = |e: std::io::Error| IndexError::Compress(e.to_string());
    let mut enc = zstd::Encoder::new(Vec::with_capacity(raw.len() / 4), level).map_err(fail)?;
    enc.include_checksum(true).map_err(fail)?;
    enc.write_all(raw).map_err(fail)?;
    enc.finish().map_err(fail)
}

/// Serialises `data` into the framed on-disk form, uncompressed.
///
/// This is the form the header describes and the form a reader parses;
/// [`to_bytes_with`] is how a writer asks for it to be compressed on the way
/// out.
pub fn to_bytes(data: &IndexData) -> Result<Vec<u8>> {
    let payload = serialise(data)?;
    let trailer = serialise_provenance(&data.provenance)?;

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
///
/// Compressed, at [`DEFAULT_ZSTD_LEVEL`]. The name does not change — a
/// compressed `index-<rev>.bin` is still an `index-<rev>.bin` — and
/// [`IndexFile::open`] reads either form, so a store or cache holding files
/// from before this change keeps working. Use [`write_index_with`] to write
/// the raw form deliberately.
pub fn write_index(path: &Path, data: &IndexData) -> Result<()> {
    write_index_with(path, data, Compression::default())
}

/// As [`write_index`], with the on-disk form chosen explicitly.
pub fn write_index_with(path: &Path, data: &IndexData, compression: Compression) -> Result<()> {
    let bytes = to_bytes_with(data, compression)?;
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

#[derive(Debug)]
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
    let bad_header = |reason: &str| IndexError::BadHeader {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    };
    if p_off != PREFIX_LEN as u64 {
        return Err(bad_header(&format!(
            "payload offset {p_off} != PREFIX_LEN ({PREFIX_LEN})"
        )));
    }
    if t_off % ALIGN as u64 != 0 {
        return Err(bad_header(&format!(
            "trailer offset {t_off} is not {ALIGN}-byte aligned"
        )));
    }
    let trailer_end = t_off
        .checked_add(t_len)
        .ok_or_else(|| bad_header("offset/length overflow"))?;
    let payload_end = p_off
        .checked_add(p_len)
        .ok_or_else(|| bad_header("offset/length overflow"))?;
    let want = trailer_end.max(payload_end);
    if want > file_len {
        return Err(IndexError::Truncated {
            path: path.to_path_buf(),
            want,
            have: file_len,
        });
    }
    let p_off = usize::try_from(p_off).map_err(|_| bad_header("payload offset overflows usize"))?;
    let p_len = usize::try_from(p_len).map_err(|_| bad_header("payload length overflows usize"))?;
    let t_off = usize::try_from(t_off).map_err(|_| bad_header("trailer offset overflows usize"))?;
    let t_len = usize::try_from(t_len).map_err(|_| bad_header("trailer length overflows usize"))?;
    Ok(Frame {
        payload: (p_off, p_len),
        trailer: (t_off, t_len),
    })
}

/// A `std::io::Write` sink that collects into rkyv's aligned buffer, so a
/// decompressed index lands somewhere the archive can be accessed in place
/// rather than being copied again out of a `Vec`.
struct AlignedSink(rkyv::util::AlignedVec<16>);

impl std::io::Write for AlignedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Decompresses one zstd frame into an aligned buffer.
///
/// A truncated or corrupt frame is an error here, not a panic downstream: the
/// framed prefix is never even looked at.
fn decompress(path: &Path, reader: impl std::io::Read) -> Result<rkyv::util::AlignedVec<16>> {
    let mut sink = AlignedSink(rkyv::util::AlignedVec::<16>::new());
    zstd::stream::copy_decode(reader, &mut sink).map_err(|e| IndexError::Decompress {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    Ok(sink.0)
}

/// Where an open index's decompressed bytes live.
///
/// `Mapped` is the raw file, mapped; `Owned` is the decompressed contents of a
/// zstd frame. Both start 16-byte aligned — a mapping starts page-aligned and
/// `AlignedVec<16>` is aligned by construction — which is what lets the
/// payload be cast in place at `PREFIX_LEN`.
enum Backing {
    Mapped(Mmap),
    Owned(rkyv::util::AlignedVec<16>),
}

impl std::ops::Deref for Backing {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Backing::Mapped(m) => m,
            Backing::Owned(v) => v,
        }
    }
}

/// An open index file: the raw layout memory-mapped, or a zstd frame
/// decompressed into an owned buffer. Views borrow from whichever it is.
pub struct IndexFile {
    path: PathBuf,
    map: Backing,
    frame: Frame,
    provenance: Provenance,
    on_disk: u64,
}

impl IndexFile {
    /// Opens `path`, in either on-disk form.
    ///
    /// A file starting with [`ZSTD_MAGIC`] is decompressed once, whole, into
    /// an owned aligned buffer and parsed from there; anything else is
    /// memory-mapped and parsed in place, exactly as before compression
    /// existed. The parsed view borrows from the mapping or the buffer either
    /// way, so nothing copies the payload a second time.
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
        let on_disk = file_len;
        let (map, file_len) = if is_zstd(&map) {
            let owned = decompress(path, &map[..])?;
            let len = owned.len() as u64;
            (Backing::Owned(owned), len)
        } else {
            (Backing::Mapped(map), file_len)
        };
        let frame = check_prefix(path, &map, file_len)?;
        // The payload must land on an alignment boundary the archive can be
        // accessed at directly out of the mapping: `check_prefix` already
        // requires `p_off == PREFIX_LEN`, and the backing buffer itself starts
        // aligned — a mapping page-aligned, an `AlignedVec<16>` by
        // construction — so this can only fail if those invariants regress.
        if (map.as_ptr() as usize + frame.payload.0) % ALIGN != 0 {
            return Err(IndexError::BadHeader {
                path: path.to_path_buf(),
                reason: format!("payload is not {ALIGN}-byte aligned"),
            });
        }
        let trailer = &map[frame.trailer.0..frame.trailer.0 + frame.trailer.1];
        let archived_trailer =
            rkyv::access::<crate::data::ArchivedProvenance, rkyv::rancor::Error>(trailer).map_err(
                |e| IndexError::Deserialize {
                    path: path.to_path_buf(),
                    reason: e.to_string(),
                },
            )?;
        let provenance = rkyv::deserialize::<Provenance, rkyv::rancor::Error>(archived_trailer)
            .map_err(|e| IndexError::Deserialize {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?;
        let me = Self {
            path: path.to_path_buf(),
            map,
            frame,
            provenance,
            on_disk,
        };
        // Once, here, rather than on every id-to-name lookup; see
        // `StrTabRef::validate`.
        me.view()
            .validate_strings()
            .map_err(|reason| IndexError::Deserialize {
                path: path.to_path_buf(),
                reason,
            })?;
        Ok(me)
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
        // alignment; the backing buffer starts 16-byte aligned (a mapping is
        // page-aligned, an `AlignedVec<16>` aligned by construction) and
        // PREFIX_LEN is 64.
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

    /// Bytes the file actually occupies, compressed or not — what `index
    /// inspect` reports as the size on disk.
    pub fn file_size(&self) -> usize {
        self.on_disk as usize
    }

    /// Bytes of the framed layout, i.e. `file_size` for a raw file and the
    /// decompressed length for a compressed one.
    pub fn framed_size(&self) -> usize {
        self.map.len()
    }

    /// Whether the file on disk was a zstd frame.
    pub fn is_compressed(&self) -> bool {
        matches!(self.map, Backing::Owned(_))
    }
}

/// Loads an index from a byte buffer, for callers that already have the bytes.
pub struct OwnedIndexBytes {
    bytes: rkyv::util::AlignedVec<16>,
    provenance: Provenance,
}

impl OwnedIndexBytes {
    /// Loads from `raw`, which may be the framed layout or one zstd frame
    /// containing it — the same either-form rule as [`IndexFile::open`].
    pub fn new(raw: &[u8]) -> Result<Self> {
        let decompressed;
        let raw = if is_zstd(raw) {
            decompressed = decompress(Path::new("<memory>"), raw)?;
            &decompressed[..]
        } else {
            raw
        };
        let frame = check_prefix(Path::new("<memory>"), raw, raw.len() as u64)?;
        let mut bytes = rkyv::util::AlignedVec::<16>::with_capacity(frame.payload.1);
        bytes.extend_from_slice(&raw[frame.payload.0..frame.payload.0 + frame.payload.1]);
        let mut trailer = rkyv::util::AlignedVec::<16>::with_capacity(frame.trailer.1);
        trailer.extend_from_slice(&raw[frame.trailer.0..frame.trailer.0 + frame.trailer.1]);
        let archived_trailer =
            rkyv::access::<crate::data::ArchivedProvenance, rkyv::rancor::Error>(&trailer)
                .map_err(|e| IndexError::Deserialize {
                    path: PathBuf::from("<memory>"),
                    reason: e.to_string(),
                })?;
        let provenance = rkyv::deserialize::<Provenance, rkyv::rancor::Error>(archived_trailer)
            .map_err(|e| IndexError::Deserialize {
                path: PathBuf::from("<memory>"),
                reason: e.to_string(),
            })?;
        let me = Self { bytes, provenance };
        me.view()
            .validate_strings()
            .map_err(|reason| IndexError::Deserialize {
                path: PathBuf::from("<memory>"),
                reason,
            })?;
        Ok(me)
    }

    pub fn archived(&self) -> &ArchivedIndexData {
        // SAFETY: as `IndexFile::archived`, with alignment from AlignedVec.
        unsafe { rkyv::access_unchecked::<ArchivedIndexData>(&self.bytes) }
    }

    pub fn view(&self) -> IndexRef<'_> {
        self.archived().view_with(&self.provenance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{new_lineage, IndexBuilder};

    fn tiny() -> Vec<u8> {
        let mut b = IndexBuilder::new(new_lineage());
        b.set_bazel_version("test");
        let data = b.finish("deadbeef");
        to_bytes(&data).unwrap()
    }

    #[test]
    fn round_trips_a_tiny_fixture() {
        let bytes = tiny();
        let frame = check_prefix(Path::new("<memory>"), &bytes, bytes.len() as u64).unwrap();
        assert_eq!(frame.payload.0, PREFIX_LEN);
    }

    #[test]
    fn rejects_a_truncated_file() {
        let bytes = tiny();
        let short = &bytes[..bytes.len() - 1];
        let err = check_prefix(Path::new("x"), short, short.len() as u64).unwrap_err();
        assert!(matches!(err, IndexError::Truncated { .. }), "{err:?}");
    }

    #[test]
    fn rejects_a_bad_version() {
        let mut bytes = tiny();
        bytes[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let err = check_prefix(Path::new("x"), &bytes, bytes.len() as u64).unwrap_err();
        assert!(matches!(err, IndexError::BadVersion { .. }), "{err:?}");
    }

    #[test]
    fn rejects_a_payload_offset_that_is_not_prefix_len() {
        let mut bytes = tiny();
        bytes[16..24].copy_from_slice(&((PREFIX_LEN as u64) + 16).to_le_bytes());
        let err = check_prefix(Path::new("x"), &bytes, bytes.len() as u64).unwrap_err();
        assert!(matches!(err, IndexError::BadHeader { .. }), "{err:?}");
    }

    #[test]
    fn rejects_a_misaligned_trailer_offset() {
        let mut bytes = tiny();
        // Nudge the trailer offset off its 16-byte alignment; keep it within
        // bounds so this exercises the alignment check specifically.
        let bad = bytes[32..40]
            .try_into()
            .map(u64::from_le_bytes)
            .unwrap()
            .saturating_sub(1);
        bytes[32..40].copy_from_slice(&bad.to_le_bytes());
        let err = check_prefix(Path::new("x"), &bytes, bytes.len() as u64).unwrap_err();
        assert!(matches!(err, IndexError::BadHeader { .. }), "{err:?}");
    }

    #[test]
    fn rejects_an_overflowing_offset() {
        let mut bytes = tiny();
        bytes[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
        let err = check_prefix(Path::new("x"), &bytes, bytes.len() as u64).unwrap_err();
        assert!(matches!(err, IndexError::BadHeader { .. }), "{err:?}");
    }
}
