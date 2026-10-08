//! Immutable, atomically published generation pack.
//!
//! Layout: fixed header, aligned opaque records, trailing directory, fixed footer. The footer
//! authenticates the directory; every directory entry authenticates its record. This module is
//! used by snapshot storage for independently readable generation components.

use memmap2::Mmap;
use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};
use thiserror::Error;

const HEADER_MAGIC: &[u8; 8] = b"RAVELPK\0";
const DIRECTORY_MAGIC: &[u8; 8] = b"RAVLDIR\0";
const FOOTER_MAGIC: &[u8; 8] = b"RAVLFTR\0";
/// 2: directory entries carry an uncompressed length, so a record can be stored
/// zstd-compressed while readers still know how much it expands to. Records whose
/// consumer borrows them zero-copy out of the mmap stay stored raw.
const VERSION: u32 = 2;
const ALIGNMENT: u32 = 16;
const HEADER_LEN: u64 = 16;
const FOOTER_LEN: u64 = 56;
const MAX_DIRECTORY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_RECORDS: u32 = 10_000_000;
const MAX_KEY_BYTES: usize = 16 * 1024;
/// zstd level 3. Measured on real payloads: 8.2x at 590 MB/s on one core, and the
/// higher levels buy single-digit percent for several times the index-time cost.
const COMPRESSION_LEVEL: i32 = 3;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum PackError {
    #[error("pack I/O at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("invalid generation pack at {path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("duplicate pack record key: {0}")]
    DuplicateKey(String),
    #[error("pack record {key} is {actual} bytes, exceeding read limit {limit}")]
    RecordTooLarge {
        key: String,
        actual: u64,
        limit: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    offset: u64,
    /// Bytes occupied in the pack — the compressed size when `plain_len` differs.
    len: u64,
    /// Size after decompression, equal to `len` for a record stored raw.
    plain_len: u64,
    /// blake3 of the bytes as stored, so verifying never has to decompress.
    checksum: [u8; 32],
}

impl Entry {
    fn compressed(&self) -> bool {
        self.plain_len != self.len
    }
}

/// Writes records immediately instead of retaining every serialized component until publish.
/// Large structural generations otherwise hold universe, reverse shards, and graph shards twice:
/// once as Rust values and once as `Vec<u8>` records.
pub(crate) struct StreamingGenerationPackWriter {
    path: PathBuf,
    parent: PathBuf,
    tmp: PathBuf,
    writer: BufWriter<fs::File>,
    position: u64,
    entries: BTreeMap<String, Entry>,
    replace_on_publish: bool,
}

impl StreamingGenerationPackWriter {
    pub(crate) fn new(path: impl AsRef<Path>) -> Result<Self, PackError> {
        let path = path.as_ref().to_path_buf();
        let parent = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        fs::create_dir_all(&parent).map_err(|source| PackError::Io {
            path: parent.clone(),
            source,
        })?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("pack.tmp-{}-{sequence}", std::process::id()));
        let file = fs::File::create(&tmp).map_err(|source| PackError::Io {
            path: tmp.clone(),
            source,
        })?;
        let mut writer = BufWriter::new(file);
        writer
            .write_all(HEADER_MAGIC)
            .and_then(|_| writer.write_all(&VERSION.to_le_bytes()))
            .and_then(|_| writer.write_all(&ALIGNMENT.to_le_bytes()))
            .map_err(|source| PackError::Io {
                path: tmp.clone(),
                source,
            })?;
        Ok(Self {
            path,
            parent,
            tmp,
            writer,
            position: HEADER_LEN,
            entries: BTreeMap::new(),
            replace_on_publish: true,
        })
    }

    pub(crate) fn add(
        &mut self,
        key: impl Into<String>,
        bytes: impl AsRef<[u8]>,
    ) -> Result<(), PackError> {
        self.add_record(key, bytes.as_ref(), false)
    }

    /// Store a record zstd-compressed.
    ///
    /// For payloads whose reader deserializes them anyway — every bincode record —
    /// this is close to free on read and cuts the pack severalfold: they are long
    /// runs of repeated path and symbol strings. Records borrowed zero-copy out of
    /// the mmap must not use this; `with_record_for_validation` refuses them rather
    /// than hand over bytes the consumer would misread as its own format.
    pub(crate) fn add_compressed(
        &mut self,
        key: impl Into<String>,
        bytes: impl AsRef<[u8]>,
    ) -> Result<(), PackError> {
        self.add_record(key, bytes.as_ref(), true)
    }

    /// Store bytes the caller already compressed.
    ///
    /// Compression is CPU-bound and this write loop is sequential, so stages that
    /// already fan out their serialization compress there and hand the result over.
    pub(crate) fn add_precompressed(
        &mut self,
        key: impl Into<String>,
        compressed: &[u8],
        plain_len: u64,
    ) -> Result<(), PackError> {
        self.add_encoded(key, compressed, plain_len)
    }

    fn add_record(
        &mut self,
        key: impl Into<String>,
        plain: &[u8],
        compress: bool,
    ) -> Result<(), PackError> {
        let encoded = if compress {
            Some(compress_record(plain, &self.tmp)?)
        } else {
            None
        };
        // A payload that does not shrink is stored raw: `plain_len == len` is exactly
        // how a reader tells the two apart.
        match encoded.as_deref() {
            Some(encoded) if encoded.len() < plain.len() => {
                self.add_encoded(key, encoded, plain.len() as u64)
            }
            _ => self.add_encoded(key, plain, plain.len() as u64),
        }
    }

    fn add_encoded(
        &mut self,
        key: impl Into<String>,
        bytes: &[u8],
        plain_len: u64,
    ) -> Result<(), PackError> {
        let key = key.into();
        if key.is_empty() || key.len() > MAX_KEY_BYTES {
            return Err(PackError::Invalid {
                path: self.path.clone(),
                message: format!("record key length must be 1..={MAX_KEY_BYTES}"),
            });
        }
        if self.entries.contains_key(&key) {
            return Err(PackError::DuplicateKey(key));
        }
        let padding = padding_for(self.position, u64::from(ALIGNMENT));
        if padding != 0 {
            self.writer
                .write_all(&[0; ALIGNMENT as usize][..padding as usize])
                .map_err(|source| PackError::Io {
                    path: self.tmp.clone(),
                    source,
                })?;
            self.position += padding;
        }
        if plain_len < bytes.len() as u64 {
            return Err(PackError::Invalid {
                path: self.path.clone(),
                message: "record expands to fewer bytes than it occupies".into(),
            });
        }
        let len = bytes.len() as u64;
        self.writer
            .write_all(bytes)
            .map_err(|source| PackError::Io {
                path: self.tmp.clone(),
                source,
            })?;
        self.entries.insert(
            key,
            Entry {
                offset: self.position,
                len,
                plain_len,
                checksum: *blake3::hash(bytes).as_bytes(),
            },
        );
        self.position = self
            .position
            .checked_add(len)
            .ok_or_else(|| PackError::Invalid {
                path: self.path.clone(),
                message: "pack offset overflow".into(),
            })?;
        Ok(())
    }

    pub(crate) fn publish(mut self) -> Result<(), PackError> {
        let directory = encode_directory(&self.entries, &self.tmp)?;
        self.writer
            .write_all(&directory)
            .and_then(|_| self.writer.write_all(FOOTER_MAGIC))
            .and_then(|_| self.writer.write_all(&self.position.to_le_bytes()))
            .and_then(|_| {
                self.writer
                    .write_all(&(directory.len() as u64).to_le_bytes())
            })
            .and_then(|_| self.writer.write_all(blake3::hash(&directory).as_bytes()))
            .and_then(|_| self.writer.flush())
            .map_err(|source| PackError::Io {
                path: self.tmp.clone(),
                source,
            })?;
        self.writer
            .get_ref()
            .sync_data()
            .map_err(|source| PackError::Io {
                path: self.tmp.clone(),
                source,
            })?;
        drop(self.writer);
        if self.replace_on_publish {
            crate::durable_io::atomic_replace(&self.tmp, &self.path).map_err(|source| {
                PackError::Io {
                    path: self.path.clone(),
                    source,
                }
            })?;
        }
        crate::durable_io::sync_parent_directory(&self.path).map_err(|source| PackError::Io {
            path: self.parent,
            source,
        })
    }
}

#[derive(Debug)]
pub struct GenerationPackReader {
    path: PathBuf,
    mmap: Mmap,
    /// Directory entries in key order. Keys stay in the mapped directory and are compared as
    /// bytes, which orders UTF-8 exactly as `String` does. Every CLI command opens the pack at
    /// least once, and decoding the directory into an owned map allocated a key per record --
    /// tens of thousands on a large workspace, 16ms per open before any record was read.
    entries: Vec<DirectoryEntry>,
    directory_offset: u64,
}

#[derive(Debug)]
struct DirectoryEntry {
    /// Byte range of the key inside the mapped file, validated as UTF-8 when decoded.
    key: std::ops::Range<usize>,
    entry: Entry,
}

/// Which file a registered reader maps. The inode tells a pack replaced by rename from the one a
/// reader holds; length and modification time tell one rewritten in place under the same name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PackIdentity {
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64),
}

impl PackIdentity {
    fn of(path: &Path, metadata: &fs::Metadata) -> Self {
        Self {
            path: path.to_path_buf(),
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt;
                (metadata.dev(), metadata.ino())
            },
        }
    }
}

/// One slot per pack this process has mapped. The slot is locked while its reader is built, so
/// loaders racing for the same pack open it once and the rest wait for that result.
type PackSlot = Arc<Mutex<Weak<GenerationPackReader>>>;

/// Packs mapped by this process. Weak, so a pack still unmaps when its last user lets go.
static OPEN_PACKS: Mutex<Vec<(PackIdentity, PackSlot)>> = Mutex::new(Vec::new());

impl GenerationPackReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PackError> {
        let path = path.as_ref().to_path_buf();
        let (file, metadata) = Self::open_file(&path)?;
        Self::map(path, &file, metadata.len())
    }

    /// [`open`](Self::open), except that a pack some other loader of this process has already
    /// mapped and still holds is reused instead of mapped, hashed and decoded again.
    ///
    /// A cold query opens the same pack several times over -- the symbol dictionary, the term
    /// index, the graph and the symbol metadata each live in it, and each loader used to pay for
    /// the directory (a checksum and a decode proportional to the record count) on its own. The
    /// pack is immutable and every holder keeps its own generation guard, so sharing the mapping
    /// changes no lifetime: the last holder still unmaps it.
    pub fn open_shared(path: impl AsRef<Path>) -> Result<Arc<Self>, PackError> {
        let path = path.as_ref().to_path_buf();
        let (file, metadata) = Self::open_file(&path)?;
        let identity = PackIdentity::of(&path, &metadata);
        let slot = {
            let mut packs = OPEN_PACKS.lock().unwrap_or_else(|e| e.into_inner());
            // Forget packs nobody holds any more, so the list stays as long as the live set.
            packs.retain(|(_, slot)| {
                slot.try_lock().map_or(true, |weak| {
                    weak.strong_count() > 0 || Arc::strong_count(slot) > 1
                })
            });
            match packs.iter().find(|(known, _)| *known == identity) {
                Some((_, slot)) => Arc::clone(slot),
                None => {
                    let slot = PackSlot::default();
                    packs.push((identity, Arc::clone(&slot)));
                    slot
                }
            }
        };
        let mut weak = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(reader) = weak.upgrade() {
            return Ok(reader);
        }
        let reader = Arc::new(Self::map(path, &file, metadata.len())?);
        *weak = Arc::downgrade(&reader);
        Ok(reader)
    }

    fn open_file(path: &Path) -> Result<(fs::File, fs::Metadata), PackError> {
        let file = fs::File::open(path).map_err(|source| PackError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let metadata = file.metadata().map_err(|source| PackError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok((file, metadata))
    }

    fn map(path: PathBuf, file: &fs::File, file_len: u64) -> Result<Self, PackError> {
        if file_len < HEADER_LEN + FOOTER_LEN {
            return invalid(&path, "truncated header/footer");
        }
        // SAFETY: the immutable generation file is protected by a generation lease while any
        // reader is alive. Writers publish a new path and never modify a referenced pack.
        let mmap = unsafe { Mmap::map(file) }.map_err(|source| PackError::Io {
            path: path.clone(),
            source,
        })?;
        let header = &mmap[..HEADER_LEN as usize];
        if &header[..8] != HEADER_MAGIC
            || u32_at(header, 8) != VERSION
            || u32_at(header, 12) != ALIGNMENT
        {
            return invalid(&path, "unsupported header");
        }
        let footer: [u8; FOOTER_LEN as usize] = mmap
            [(file_len - FOOTER_LEN) as usize..file_len as usize]
            .try_into()
            .expect("footer length was checked");
        (|| {
            if &footer[..8] != FOOTER_MAGIC {
                return invalid(&path, "missing footer magic");
            }
            let directory_offset = u64_at(&footer, 8);
            let directory_len = u64_at(&footer, 16);
            if directory_len > MAX_DIRECTORY_BYTES
                || directory_offset < HEADER_LEN
                || directory_offset.checked_add(directory_len) != Some(file_len - FOOTER_LEN)
            {
                return invalid(&path, "directory bounds are invalid");
            }
            let directory =
                &mmap[directory_offset as usize..(directory_offset + directory_len) as usize];
            if blake3::hash(directory).as_bytes() != &footer[24..56] {
                return invalid(&path, "directory checksum mismatch");
            }
            let entries = decode_directory(directory, directory_offset, &path)?;
            Ok(Self {
                path,
                mmap,
                entries,
                directory_offset,
            })
        })()
    }

    fn key_bytes(&self, entry: &DirectoryEntry) -> &[u8] {
        &self.mmap[entry.key.clone()]
    }

    fn entry(&self, key: &str) -> Option<&Entry> {
        self.entries
            .binary_search_by(|candidate| self.key_bytes(candidate).cmp(key.as_bytes()))
            .ok()
            .map(|index| &self.entries[index].entry)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| {
            std::str::from_utf8(self.key_bytes(entry)).expect("keys are validated when decoded")
        })
    }

    /// Takes `&self`: reading only borrows the mmap and the decoded directory, so a
    /// reader can be shared and opened once instead of per record.
    pub fn read(&self, key: &str, max_bytes: u64) -> Result<Option<Vec<u8>>, PackError> {
        let Some(entry) = self.entry(key) else {
            return Ok(None);
        };
        // Bound the expanded size: that is what the caller ends up holding.
        if entry.plain_len > max_bytes || entry.plain_len > usize::MAX as u64 {
            return Err(PackError::RecordTooLarge {
                key: key.into(),
                actual: entry.plain_len,
                limit: max_bytes,
            });
        }
        if entry
            .offset
            .checked_add(entry.len)
            .is_none_or(|end| end > self.directory_offset)
        {
            return invalid(&self.path, "record bounds are invalid");
        }
        let bytes = &self.mmap[entry.offset as usize..(entry.offset + entry.len) as usize];
        let checksum = if bytes.len() >= 1024 * 1024 {
            let mut hasher = blake3::Hasher::new();
            hasher.update_rayon(bytes);
            hasher.finalize()
        } else {
            blake3::hash(bytes)
        };
        if checksum.as_bytes() != &entry.checksum {
            return invalid(&self.path, "record checksum mismatch");
        }
        if !entry.compressed() {
            return Ok(Some(bytes.to_vec()));
        }
        let plain = decompress_record(bytes, entry.plain_len, &self.path)?;
        Ok(Some(plain))
    }

    pub fn with_record<T>(
        &self,
        key: &str,
        max_bytes: u64,
        read: impl FnOnce(&[u8]) -> T,
    ) -> Result<Option<T>, PackError> {
        let Some(entry) = self.entry(key) else {
            return Ok(None);
        };
        if entry.plain_len > max_bytes || entry.plain_len > usize::MAX as u64 {
            return Err(PackError::RecordTooLarge {
                key: key.into(),
                actual: entry.plain_len,
                limit: max_bytes,
            });
        }
        let end = entry
            .offset
            .checked_add(entry.len)
            .ok_or_else(|| PackError::Invalid {
                path: self.path.clone(),
                message: "record bounds overflow".into(),
            })?;
        if end > self.directory_offset {
            return invalid(&self.path, "record bounds are invalid");
        }
        let bytes = &self.mmap[entry.offset as usize..end as usize];
        if blake3::hash(bytes).as_bytes() != &entry.checksum {
            return invalid(&self.path, "record checksum mismatch");
        }
        if !entry.compressed() {
            return Ok(Some(read(bytes)));
        }
        let plain = decompress_record(bytes, entry.plain_len, &self.path)?;
        Ok(Some(read(&plain)))
    }

    /// Borrow a bounded record without recomputing its blake3 checksum. The consumer must fully
    /// validate the record format before interpreting it; this is intended for bytecheck/rkyv
    /// hot paths. `ravel validate` still verifies the stored checksum separately.
    pub(crate) fn with_record_for_validation<T>(
        &self,
        key: &str,
        max_bytes: u64,
        validate: impl FnOnce(&[u8]) -> T,
    ) -> Result<Option<T>, PackError> {
        let Some(entry) = self.entry(key) else {
            return Ok(None);
        };
        // Refuse instead of handing back compressed bytes: this path exists so the
        // consumer can borrow the record straight out of the mmap, and it would read
        // the compressed form as its own format.
        if entry.compressed() {
            return invalid(
                &self.path,
                "record is stored compressed and cannot be borrowed for zero-copy access",
            );
        }
        if entry.len > max_bytes || entry.len > usize::MAX as u64 {
            return Err(PackError::RecordTooLarge {
                key: key.into(),
                actual: entry.len,
                limit: max_bytes,
            });
        }
        let end = entry
            .offset
            .checked_add(entry.len)
            .ok_or_else(|| PackError::Invalid {
                path: self.path.clone(),
                message: "record bounds overflow".into(),
            })?;
        if end > self.directory_offset {
            return invalid(&self.path, "record bounds are invalid");
        }
        Ok(Some(validate(
            &self.mmap[entry.offset as usize..end as usize],
        )))
    }
}

fn encode_directory(entries: &BTreeMap<String, Entry>, path: &Path) -> Result<Vec<u8>, PackError> {
    if entries.len() > MAX_RECORDS as usize {
        return invalid(path, "too many records");
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(DIRECTORY_MAGIC);
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (key, entry) in entries {
        bytes.extend_from_slice(&(key.len() as u32).to_le_bytes());
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(&entry.offset.to_le_bytes());
        bytes.extend_from_slice(&entry.len.to_le_bytes());
        bytes.extend_from_slice(&entry.plain_len.to_le_bytes());
        bytes.extend_from_slice(&entry.checksum);
        if bytes.len() as u64 > MAX_DIRECTORY_BYTES {
            return invalid(path, "directory exceeds size limit");
        }
    }
    Ok(bytes)
}

fn decode_directory(
    bytes: &[u8],
    records_end: u64,
    path: &Path,
) -> Result<Vec<DirectoryEntry>, PackError> {
    if bytes.len() < 12 || &bytes[..8] != DIRECTORY_MAGIC {
        return invalid(path, "invalid directory header");
    }
    let count = u32_at(bytes, 8);
    if count > MAX_RECORDS {
        return invalid(path, "record count exceeds limit");
    }
    // The directory starts where the records end, so a key's position in `bytes` maps to the
    // file offset `records_end + position`.
    let base = records_end as usize;
    let mut cursor = 12usize;
    let mut entries = Vec::with_capacity(count as usize);
    // Writers encode the directory from a sorted map, so it normally arrives strictly ordered;
    // anything else is sorted below and still checked for duplicates.
    let mut ordered = true;
    for _ in 0..count {
        let key_len = take_u32(bytes, &mut cursor, path)? as usize;
        if key_len == 0
            || key_len > MAX_KEY_BYTES
            // key + offset(8) + len(8) + plain_len(8) + checksum(32)
            || cursor
                .checked_add(key_len + 56)
                .is_none_or(|end| end > bytes.len())
        {
            return invalid(path, "directory entry bounds are invalid");
        }
        let key = &bytes[cursor..cursor + key_len];
        if std::str::from_utf8(key).is_err() {
            return invalid(path, "directory key is not UTF-8");
        }
        if let Some(previous) = entries
            .last()
            .map(|entry: &DirectoryEntry| &bytes[entry.key.start - base..entry.key.end - base])
        {
            match previous.cmp(key) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => {
                    return Err(PackError::DuplicateKey(
                        String::from_utf8_lossy(key).into_owned(),
                    ));
                }
                std::cmp::Ordering::Greater => ordered = false,
            }
        }
        let key = base + cursor..base + cursor + key_len;
        cursor += key_len;
        let offset = take_u64(bytes, &mut cursor, path)?;
        let len = take_u64(bytes, &mut cursor, path)?;
        let plain_len = take_u64(bytes, &mut cursor, path)?;
        let checksum: [u8; 32] = bytes[cursor..cursor + 32].try_into().unwrap();
        cursor += 32;
        if offset % u64::from(ALIGNMENT) != 0
            || offset < HEADER_LEN
            || offset.checked_add(len).is_none_or(|end| end > records_end)
        {
            return invalid(path, "record points outside data region");
        }
        entries.push(DirectoryEntry {
            key,
            entry: Entry {
                offset,
                len,
                plain_len,
                checksum,
            },
        });
    }
    if cursor != bytes.len() {
        return invalid(path, "trailing directory bytes");
    }
    if !ordered {
        let key_of = |entry: &DirectoryEntry| &bytes[entry.key.start - base..entry.key.end - base];
        entries.sort_by(|left, right| key_of(left).cmp(key_of(right)));
        if let Some(pair) = entries
            .windows(2)
            .find(|pair| key_of(&pair[0]) == key_of(&pair[1]))
        {
            return Err(PackError::DuplicateKey(
                String::from_utf8_lossy(key_of(&pair[0])).into_owned(),
            ));
        }
    }
    Ok(entries)
}

thread_local! {
    // One zstd context per thread, reused for every record. `zstd::bulk::{compress, decompress}`
    // create and free a context per call, and a full index compresses tens of thousands of shards:
    // each call allocated (and the kernel zeroed) a fresh workspace. `compress2` starts a new frame
    // from the stored parameters every time, so a reused context writes the same bytes.
    static COMPRESSOR: std::cell::RefCell<Option<zstd::bulk::Compressor<'static>>> =
        const { std::cell::RefCell::new(None) };
    static DECOMPRESSOR: std::cell::RefCell<Option<zstd::bulk::Decompressor<'static>>> =
        const { std::cell::RefCell::new(None) };
}

/// Expand a compressed record, rejecting a length that disagrees with the directory.
fn decompress_record(bytes: &[u8], plain_len: u64, path: &Path) -> Result<Vec<u8>, PackError> {
    let plain = DECOMPRESSOR
        .with(|cell| {
            let mut slot = cell.borrow_mut();
            let decompressor = match slot.as_mut() {
                Some(decompressor) => decompressor,
                None => slot.insert(zstd::bulk::Decompressor::new()?),
            };
            let result = decompressor.decompress(bytes, plain_len as usize);
            if result.is_err() {
                // Never carry a context out of a failed frame into the next record.
                *slot = None;
            }
            result
        })
        .map_err(|source| PackError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if plain.len() as u64 != plain_len {
        return Err(PackError::Invalid {
            path: path.to_path_buf(),
            message: "record expanded to an unexpected length".into(),
        });
    }
    Ok(plain)
}

/// zstd a record, reporting the pack path on failure.
///
/// The result is trimmed to its length: `Compressor::compress` sizes its buffer for the worst case
/// (slightly over the plain size) and staging holds thousands of compressed records at once, so the
/// unused tails added up to roughly the uncompressed size of everything staged.
pub(crate) fn compress_record(plain: &[u8], path: &Path) -> Result<Vec<u8>, PackError> {
    COMPRESSOR
        .with(|cell| {
            let mut slot = cell.borrow_mut();
            let compressor = match slot.as_mut() {
                Some(compressor) => compressor,
                None => slot.insert(zstd::bulk::Compressor::new(COMPRESSION_LEVEL)?),
            };
            let result = compressor.compress(plain);
            if result.is_err() {
                *slot = None;
            }
            let mut compressed = result?;
            compressed.shrink_to_fit();
            Ok(compressed)
        })
        .map_err(|source| PackError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn padding_for(position: u64, alignment: u64) -> u64 {
    (alignment - position % alignment) % alignment
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}
fn take_u32(bytes: &[u8], cursor: &mut usize, path: &Path) -> Result<u32, PackError> {
    if cursor.checked_add(4).is_none_or(|end| end > bytes.len()) {
        return invalid(path, "truncated directory");
    }
    let value = u32_at(bytes, *cursor);
    *cursor += 4;
    Ok(value)
}
fn take_u64(bytes: &[u8], cursor: &mut usize, path: &Path) -> Result<u64, PackError> {
    if cursor.checked_add(8).is_none_or(|end| end > bytes.len()) {
        return invalid(path, "truncated directory");
    }
    let value = u64_at(bytes, *cursor);
    *cursor += 8;
    Ok(value)
}
fn invalid<T>(path: &Path, message: impl Into<String>) -> Result<T, PackError> {
    Err(PackError::Invalid {
        path: path.to_path_buf(),
        message: message.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn directory_of(keys: &[&str]) -> Vec<u8> {
        let mut bytes = DIRECTORY_MAGIC.to_vec();
        bytes.extend_from_slice(&(keys.len() as u32).to_le_bytes());
        for (index, key) in keys.iter().enumerate() {
            bytes.extend_from_slice(&(key.len() as u32).to_le_bytes());
            bytes.extend_from_slice(key.as_bytes());
            bytes.extend_from_slice(&(HEADER_LEN + 16 * index as u64).to_le_bytes());
            bytes.extend_from_slice(&1u64.to_le_bytes());
            bytes.extend_from_slice(&1u64.to_le_bytes());
            bytes.extend_from_slice(&[index as u8; 32]);
        }
        bytes
    }

    #[test]
    fn a_directory_out_of_key_order_is_sorted_and_duplicates_are_refused() {
        let path = Path::new("test.pack");
        let records_end = 1024;
        let keys_of = |entries: &[DirectoryEntry], bytes: &[u8]| -> Vec<String> {
            entries
                .iter()
                .map(|entry| {
                    let start = entry.key.start - records_end as usize;
                    String::from_utf8(bytes[start..start + entry.key.len()].to_vec()).unwrap()
                })
                .collect()
        };
        let ordered = directory_of(&["a", "b/c", "b/d"]);
        let entries = decode_directory(&ordered, records_end, path).unwrap();
        assert_eq!(keys_of(&entries, &ordered), ["a", "b/c", "b/d"]);

        // Not something this writer produces, but a reader must not binary-search it unsorted.
        let shuffled = directory_of(&["b/d", "a", "b/c"]);
        let entries = decode_directory(&shuffled, records_end, path).unwrap();
        assert_eq!(keys_of(&entries, &shuffled), ["a", "b/c", "b/d"]);
        assert_eq!(
            entries[0].entry.checksum, [1; 32],
            "entries move with their keys"
        );

        for keys in [&["a", "a"][..], &["b", "a", "b"][..]] {
            assert!(matches!(
                decode_directory(&directory_of(keys), records_end, path),
                Err(PackError::DuplicateKey(key)) if key == keys[0]
            ));
        }
    }

    #[test]
    fn roundtrip_alignment_and_bounded_reads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("generation.pack");
        let mut writer = StreamingGenerationPackWriter::new(&path).unwrap();
        writer.add("a", b"abc").unwrap();
        writer.add("large", vec![7; 100]).unwrap();
        writer.publish().unwrap();
        let reader = GenerationPackReader::open(&path).unwrap();
        assert_eq!(reader.keys().collect::<Vec<_>>(), vec!["a", "large"]);
        assert_eq!(reader.read("a", 3).unwrap().unwrap(), b"abc");
        assert!(matches!(
            reader.read("large", 99),
            Err(PackError::RecordTooLarge { .. })
        ));
        assert!(
            reader
                .entries
                .iter()
                .all(|entry| entry.entry.offset % 8 == 0)
        );
    }

    /// A compressed record must read back byte-identical, and its bound must apply to
    /// the expanded size — a caller asking for at most N bytes gets at most N bytes,
    /// not N compressed bytes that expand past its limit.
    #[test]
    fn compressed_records_roundtrip_and_bound_the_expanded_size() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("generation.pack");
        // Long runs of a repeated string: the shape every bincode payload here has.
        let compressible = "apps/banking/src/application/services/wallet.service.ts"
            .repeat(400)
            .into_bytes();
        let mut writer = StreamingGenerationPackWriter::new(&path).unwrap();
        writer.add_compressed("squashed", &compressible).unwrap();
        writer.add("plain", b"kept raw").unwrap();
        writer.publish().unwrap();

        let reader = GenerationPackReader::open(&path).unwrap();
        assert_eq!(
            reader
                .read("squashed", compressible.len() as u64)
                .unwrap()
                .unwrap(),
            compressible,
            "compressed record must expand to exactly what was written"
        );
        assert_eq!(reader.read("plain", 64).unwrap().unwrap(), b"kept raw");

        let stored = reader.entry("squashed").unwrap().len;
        assert!(
            stored < compressible.len() as u64,
            "payload should have shrunk: {stored} vs {}",
            compressible.len()
        );
        assert!(reader.entry("squashed").unwrap().compressed());
        assert!(!reader.entry("plain").unwrap().compressed());

        // The limit is checked against the expanded length.
        assert!(matches!(
            reader.read("squashed", compressible.len() as u64 - 1),
            Err(PackError::RecordTooLarge { .. })
        ));
        // And `with_record` sees the expanded bytes too.
        let seen = reader
            .with_record("squashed", compressible.len() as u64, <[u8]>::to_vec)
            .unwrap()
            .unwrap();
        assert_eq!(seen, compressible);
    }

    /// Asking to compress something that does not shrink must store it raw, so a
    /// reader never pays a decompression pass for nothing — and `plain_len == len`
    /// stays the only signal distinguishing the two.
    #[test]
    fn incompressible_payload_is_stored_raw() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("generation.pack");
        // Shorter than a zstd frame header, so the "compressed" form cannot be
        // smaller whatever the entropy — deterministic, unlike relying on data that
        // merely looks random.
        let tiny = b"xy";
        let mut writer = StreamingGenerationPackWriter::new(&path).unwrap();
        writer.add_compressed("tiny", tiny).unwrap();
        writer.publish().unwrap();
        let reader = GenerationPackReader::open(&path).unwrap();
        assert!(
            !reader.entry("tiny").unwrap().compressed(),
            "a payload that does not shrink must be stored raw"
        );
        assert_eq!(reader.read("tiny", 4096).unwrap().unwrap(), tiny);
    }

    /// The zero-copy borrow must refuse a compressed record rather than hand back
    /// bytes the consumer would interpret as its own format. Silently returning the
    /// compressed form is the failure this guards: rkyv would read it as a corrupt
    /// archive, or worse, not notice.
    #[test]
    fn zero_copy_borrow_refuses_a_compressed_record() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("generation.pack");
        let payload = "symbol://x#value:Y".repeat(200).into_bytes();
        let mut writer = StreamingGenerationPackWriter::new(&path).unwrap();
        writer.add_compressed("squashed", &payload).unwrap();
        writer.add("plain", &payload).unwrap();
        writer.publish().unwrap();
        let reader = GenerationPackReader::open(&path).unwrap();

        assert!(
            reader
                .with_record_for_validation("squashed", 1 << 20, |_| ())
                .is_err(),
            "a compressed record must not be borrowed for zero-copy access"
        );
        // The raw twin is still borrowable, so the refusal is about compression only.
        let borrowed = reader
            .with_record_for_validation("plain", 1 << 20, <[u8]>::to_vec)
            .unwrap()
            .unwrap();
        assert_eq!(borrowed, payload);
    }

    #[test]
    fn truncation_and_record_corruption_are_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("generation.pack");
        let mut writer = StreamingGenerationPackWriter::new(&path).unwrap();
        writer.add("a", b"payload").unwrap();
        writer.publish().unwrap();
        let original = fs::read(&path).unwrap();
        fs::write(&path, &original[..original.len() - 1]).unwrap();
        assert!(GenerationPackReader::open(&path).is_err());
        fs::write(&path, &original).unwrap();
        // Drop the reader before overwriting: Windows refuses to replace a file
        // while its mmap section is open (os error 1224). A record's stored
        // blake3 is verified on `read`, so a fresh reader still rejects the flip.
        let offset = {
            let reader = GenerationPackReader::open(&path).unwrap();
            reader.entry("a").unwrap().offset as usize
        };
        let mut corrupt = original;
        corrupt[offset] ^= 1;
        fs::write(&path, corrupt).unwrap();
        let reader = GenerationPackReader::open(&path).unwrap();
        assert!(reader.read("a", 100).is_err());
    }

    #[test]
    fn directory_checksum_corruption_is_rejected_before_decode() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("generation.pack");
        let mut writer = StreamingGenerationPackWriter::new(&path).unwrap();
        writer.add("a", b"payload").unwrap();
        writer.publish().unwrap();
        let mut bytes = fs::read(&path).unwrap();
        let footer = bytes.len() - FOOTER_LEN as usize;
        let directory_offset = u64_at(&bytes[footer..], 8) as usize;
        bytes[directory_offset] ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(GenerationPackReader::open(path).is_err());
    }

    fn write_pack(path: &Path, value: &[u8]) {
        let mut writer = StreamingGenerationPackWriter::new(path).unwrap();
        writer.add("value", value).unwrap();
        writer.publish().unwrap();
    }

    #[test]
    fn loaders_of_one_pack_share_a_reader_for_as_long_as_one_is_held() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("generation.pack");
        write_pack(&path, b"first");

        let first = GenerationPackReader::open_shared(&path).unwrap();
        let second = GenerationPackReader::open_shared(&path).unwrap();
        assert!(Arc::ptr_eq(&first, &second), "one mapping, not two");
        assert_eq!(second.read("value", 8).unwrap().unwrap(), b"first");

        // Sharing never extends a mapping: once the last holder lets go the registry keeps nothing
        // alive, and the next loader maps the pack afresh.
        drop((first, second));
        let still_mapped = |path: &Path| {
            OPEN_PACKS
                .lock()
                .unwrap()
                .iter()
                .filter(|(identity, slot)| {
                    identity.path == path && slot.lock().unwrap().strong_count() > 0
                })
                .count()
        };
        assert_eq!(still_mapped(&path), 0);
        let reopened = GenerationPackReader::open_shared(&path).unwrap();
        assert_eq!(reopened.read("value", 8).unwrap().unwrap(), b"first");
        assert_eq!(still_mapped(&path), 1);
    }

    #[test]
    fn a_pack_replaced_under_the_same_name_is_never_served_from_the_old_mapping() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("generation.pack");
        write_pack(&path, b"old");
        let held = GenerationPackReader::open_shared(&path).unwrap();

        // Publication is a rename onto the live name; the old inode stays mapped by `held`.
        let staged = dir.path().join("staged.pack");
        write_pack(&staged, b"new-and-longer");
        fs::rename(&staged, &path).unwrap();

        let current = GenerationPackReader::open_shared(&path).unwrap();
        assert!(!Arc::ptr_eq(&held, &current));
        assert_eq!(held.read("value", 32).unwrap().unwrap(), b"old");
        assert_eq!(
            current.read("value", 32).unwrap().unwrap(),
            b"new-and-longer"
        );
    }

    #[test]
    fn concurrent_loaders_open_a_pack_once_and_a_bad_pack_fails_each_of_them() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("generation.pack");
        write_pack(&path, b"shared");
        let readers: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| GenerationPackReader::open_shared(&path).unwrap()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(
            readers
                .iter()
                .all(|reader| Arc::ptr_eq(reader, &readers[0]))
        );
        drop(readers);

        let corrupt = dir.path().join("corrupt.pack");
        fs::write(
            &corrupt,
            b"not a pack at all, but longer than a header and a footer",
        )
        .unwrap();
        assert!(GenerationPackReader::open_shared(&corrupt).is_err());
        assert!(
            GenerationPackReader::open_shared(&corrupt).is_err(),
            "a failed open leaves nothing behind that a retry could mistake for success"
        );
    }

    #[test]
    fn incomplete_temp_never_replaces_published_pack() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("generation.pack");
        let mut writer = StreamingGenerationPackWriter::new(&path).unwrap();
        writer.add("stable", b"ok").unwrap();
        writer.publish().unwrap();
        fs::write(path.with_extension("pack.tmp-crash"), b"crash").unwrap();
        let reader = GenerationPackReader::open(path).unwrap();
        assert_eq!(reader.read("stable", 2).unwrap().unwrap(), b"ok");
    }

    #[test]
    fn repeated_publish_atomically_replaces_existing_pack() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("generation.pack");
        for payload in [b"A".as_slice(), b"B".as_slice(), b"A".as_slice()] {
            let mut writer = StreamingGenerationPackWriter::new(&path).unwrap();
            writer.add("value", payload).unwrap();
            writer.publish().unwrap();
            let reader = GenerationPackReader::open(&path).unwrap();
            assert_eq!(reader.read("value", 1).unwrap().unwrap(), payload);
        }
    }
}
