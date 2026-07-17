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
    sync::atomic::{AtomicU64, Ordering},
};
use thiserror::Error;

const HEADER_MAGIC: &[u8; 8] = b"RAVELPK\0";
const DIRECTORY_MAGIC: &[u8; 8] = b"RAVLDIR\0";
const FOOTER_MAGIC: &[u8; 8] = b"RAVLFTR\0";
const VERSION: u32 = 1;
const ALIGNMENT: u32 = 16;
const HEADER_LEN: u64 = 16;
const FOOTER_LEN: u64 = 56;
const MAX_DIRECTORY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_RECORDS: u32 = 10_000_000;
const MAX_KEY_BYTES: usize = 16 * 1024;
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
    len: u64,
    checksum: [u8; 32],
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
        let bytes = bytes.as_ref();
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
    entries: BTreeMap<String, Entry>,
    directory_offset: u64,
}

impl GenerationPackReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PackError> {
        let path = path.as_ref().to_path_buf();
        let file = fs::File::open(&path).map_err(|source| PackError::Io {
            path: path.clone(),
            source,
        })?;
        let file_len = file
            .metadata()
            .map_err(|source| PackError::Io {
                path: path.clone(),
                source,
            })?
            .len();
        if file_len < HEADER_LEN + FOOTER_LEN {
            return invalid(&path, "truncated header/footer");
        }
        // SAFETY: the immutable generation file is protected by a generation lease while any
        // reader is alive. Writers publish a new path and never modify a referenced pack.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|source| PackError::Io {
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

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    pub fn read(&mut self, key: &str, max_bytes: u64) -> Result<Option<Vec<u8>>, PackError> {
        let Some(entry) = self.entries.get(key) else {
            return Ok(None);
        };
        if entry.len > max_bytes || entry.len > usize::MAX as u64 {
            return Err(PackError::RecordTooLarge {
                key: key.into(),
                actual: entry.len,
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
        Ok(Some(bytes.to_vec()))
    }

    pub fn with_record<T>(
        &self,
        key: &str,
        max_bytes: u64,
        read: impl FnOnce(&[u8]) -> T,
    ) -> Result<Option<T>, PackError> {
        let Some(entry) = self.entries.get(key) else {
            return Ok(None);
        };
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
        let bytes = &self.mmap[entry.offset as usize..end as usize];
        if blake3::hash(bytes).as_bytes() != &entry.checksum {
            return invalid(&self.path, "record checksum mismatch");
        }
        Ok(Some(read(bytes)))
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
        let Some(entry) = self.entries.get(key) else {
            return Ok(None);
        };
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
) -> Result<BTreeMap<String, Entry>, PackError> {
    if bytes.len() < 12 || &bytes[..8] != DIRECTORY_MAGIC {
        return invalid(path, "invalid directory header");
    }
    let count = u32_at(bytes, 8);
    if count > MAX_RECORDS {
        return invalid(path, "record count exceeds limit");
    }
    let mut cursor = 12usize;
    let mut entries = BTreeMap::new();
    for _ in 0..count {
        let key_len = take_u32(bytes, &mut cursor, path)? as usize;
        if key_len == 0
            || key_len > MAX_KEY_BYTES
            || cursor
                .checked_add(key_len + 48)
                .is_none_or(|end| end > bytes.len())
        {
            return invalid(path, "directory entry bounds are invalid");
        }
        let key = std::str::from_utf8(&bytes[cursor..cursor + key_len])
            .map_err(|_| PackError::Invalid {
                path: path.to_path_buf(),
                message: "directory key is not UTF-8".into(),
            })?
            .to_owned();
        cursor += key_len;
        let offset = take_u64(bytes, &mut cursor, path)?;
        let len = take_u64(bytes, &mut cursor, path)?;
        let checksum: [u8; 32] = bytes[cursor..cursor + 32].try_into().unwrap();
        cursor += 32;
        if offset % u64::from(ALIGNMENT) != 0
            || offset < HEADER_LEN
            || offset.checked_add(len).is_none_or(|end| end > records_end)
        {
            return invalid(path, "record points outside data region");
        }
        if entries
            .insert(
                key.clone(),
                Entry {
                    offset,
                    len,
                    checksum,
                },
            )
            .is_some()
        {
            return Err(PackError::DuplicateKey(key));
        }
    }
    if cursor != bytes.len() {
        return invalid(path, "trailing directory bytes");
    }
    Ok(entries)
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

    #[test]
    fn roundtrip_alignment_and_bounded_reads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("generation.pack");
        let mut writer = StreamingGenerationPackWriter::new(&path).unwrap();
        writer.add("a", b"abc").unwrap();
        writer.add("large", vec![7; 100]).unwrap();
        writer.publish().unwrap();
        let mut reader = GenerationPackReader::open(&path).unwrap();
        assert_eq!(reader.keys().collect::<Vec<_>>(), vec!["a", "large"]);
        assert_eq!(reader.read("a", 3).unwrap().unwrap(), b"abc");
        assert!(matches!(
            reader.read("large", 99),
            Err(PackError::RecordTooLarge { .. })
        ));
        assert!(reader.entries.values().all(|entry| entry.offset % 8 == 0));
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
            reader.entries["a"].offset as usize
        };
        let mut corrupt = original;
        corrupt[offset] ^= 1;
        fs::write(&path, corrupt).unwrap();
        let mut reader = GenerationPackReader::open(&path).unwrap();
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

    #[test]
    fn incomplete_temp_never_replaces_published_pack() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("generation.pack");
        let mut writer = StreamingGenerationPackWriter::new(&path).unwrap();
        writer.add("stable", b"ok").unwrap();
        writer.publish().unwrap();
        fs::write(path.with_extension("pack.tmp-crash"), b"crash").unwrap();
        let mut reader = GenerationPackReader::open(path).unwrap();
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
            let mut reader = GenerationPackReader::open(&path).unwrap();
            assert_eq!(reader.read("value", 1).unwrap().unwrap(), payload);
        }
    }
}
