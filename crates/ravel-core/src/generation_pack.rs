//! Immutable, atomically published generation pack.
//!
//! Layout: fixed header, aligned opaque records, trailing directory, fixed footer. The footer
//! authenticates the directory; every directory entry authenticates its record. This module is
//! intentionally not wired into snapshot storage yet.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use thiserror::Error;

const HEADER_MAGIC: &[u8; 8] = b"RAVELPK\0";
const DIRECTORY_MAGIC: &[u8; 8] = b"RAVLDIR\0";
const FOOTER_MAGIC: &[u8; 8] = b"RAVLFTR\0";
const VERSION: u32 = 1;
const ALIGNMENT: u32 = 8;
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

/// Builds one immutable pack and atomically renames it into place.
pub struct GenerationPackWriter {
    records: BTreeMap<String, Vec<u8>>,
}

impl GenerationPackWriter {
    pub fn new() -> Self {
        Self {
            records: BTreeMap::new(),
        }
    }

    pub fn add(
        &mut self,
        key: impl Into<String>,
        bytes: impl Into<Vec<u8>>,
    ) -> Result<(), PackError> {
        let key = key.into();
        if key.is_empty() || key.len() > MAX_KEY_BYTES {
            return Err(PackError::Invalid {
                path: PathBuf::new(),
                message: format!("record key length must be 1..={MAX_KEY_BYTES}"),
            });
        }
        if self.records.insert(key.clone(), bytes.into()).is_some() {
            return Err(PackError::DuplicateKey(key));
        }
        Ok(())
    }

    pub fn publish(self, path: impl AsRef<Path>) -> Result<(), PackError> {
        let path = path.as_ref();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| PackError::Io {
            path: parent.to_path_buf(),
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
        let mut position = HEADER_LEN;
        let mut entries = BTreeMap::new();
        for (key, bytes) in self.records {
            let padding = padding_for(position, u64::from(ALIGNMENT));
            if padding != 0 {
                writer
                    .write_all(&[0; ALIGNMENT as usize][..padding as usize])
                    .map_err(|source| PackError::Io {
                        path: tmp.clone(),
                        source,
                    })?;
                position += padding;
            }
            let len = bytes.len() as u64;
            writer.write_all(&bytes).map_err(|source| PackError::Io {
                path: tmp.clone(),
                source,
            })?;
            entries.insert(
                key,
                Entry {
                    offset: position,
                    len,
                    checksum: *blake3::hash(&bytes).as_bytes(),
                },
            );
            position = position
                .checked_add(len)
                .ok_or_else(|| PackError::Invalid {
                    path: tmp.clone(),
                    message: "pack offset overflow".into(),
                })?;
        }

        let directory_offset = position;
        let directory = encode_directory(&entries, &tmp)?;
        writer
            .write_all(&directory)
            .map_err(|source| PackError::Io {
                path: tmp.clone(),
                source,
            })?;
        writer
            .write_all(FOOTER_MAGIC)
            .and_then(|_| writer.write_all(&directory_offset.to_le_bytes()))
            .and_then(|_| writer.write_all(&(directory.len() as u64).to_le_bytes()))
            .and_then(|_| writer.write_all(blake3::hash(&directory).as_bytes()))
            .and_then(|_| writer.flush())
            .map_err(|source| PackError::Io {
                path: tmp.clone(),
                source,
            })?;
        // Exactly one data durability barrier per pack. Rename happens only after it succeeds.
        writer
            .get_ref()
            .sync_data()
            .map_err(|source| PackError::Io {
                path: tmp.clone(),
                source,
            })?;
        drop(writer);
        fs::rename(&tmp, path).map_err(|source| PackError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| PackError::Io {
                path: parent.to_path_buf(),
                source,
            })
    }
}

impl Default for GenerationPackWriter {
    fn default() -> Self {
        Self::new()
    }
}

pub struct GenerationPackReader {
    path: PathBuf,
    file: fs::File,
    entries: BTreeMap<String, Entry>,
    directory_offset: u64,
}

impl GenerationPackReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PackError> {
        let path = path.as_ref().to_path_buf();
        let mut file = fs::File::open(&path).map_err(|source| PackError::Io {
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
        let mut header = [0u8; HEADER_LEN as usize];
        file.read_exact(&mut header)
            .map_err(|source| PackError::Io {
                path: path.clone(),
                source,
            })?;
        if &header[..8] != HEADER_MAGIC
            || u32_at(&header, 8) != VERSION
            || u32_at(&header, 12) != ALIGNMENT
        {
            return invalid(&path, "unsupported header");
        }
        file.seek(SeekFrom::Start(file_len - FOOTER_LEN))
            .and_then(|_| {
                let mut footer = [0u8; FOOTER_LEN as usize];
                file.read_exact(&mut footer).map(|_| footer)
            })
            .map_err(|source| PackError::Io {
                path: path.clone(),
                source,
            })
            .and_then(|footer| {
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
                let mut directory = vec![0; directory_len as usize];
                file.seek(SeekFrom::Start(directory_offset))
                    .and_then(|_| file.read_exact(&mut directory))
                    .map_err(|source| PackError::Io {
                        path: path.clone(),
                        source,
                    })?;
                if blake3::hash(&directory).as_bytes() != &footer[24..56] {
                    return invalid(&path, "directory checksum mismatch");
                }
                let entries = decode_directory(&directory, directory_offset, &path)?;
                Ok(Self {
                    path,
                    file,
                    entries,
                    directory_offset,
                })
            })
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
        let mut bytes = vec![0; entry.len as usize];
        self.file
            .seek(SeekFrom::Start(entry.offset))
            .and_then(|_| self.file.read_exact(&mut bytes))
            .map_err(|source| PackError::Io {
                path: self.path.clone(),
                source,
            })?;
        if blake3::hash(&bytes).as_bytes() != &entry.checksum {
            return invalid(&self.path, "record checksum mismatch");
        }
        Ok(Some(bytes))
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
        let mut writer = GenerationPackWriter::new();
        writer.add("a", b"abc".to_vec()).unwrap();
        writer.add("large", vec![7; 100]).unwrap();
        writer.publish(&path).unwrap();
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
        let mut writer = GenerationPackWriter::new();
        writer.add("a", b"payload".to_vec()).unwrap();
        writer.publish(&path).unwrap();
        let original = fs::read(&path).unwrap();
        fs::write(&path, &original[..original.len() - 1]).unwrap();
        assert!(GenerationPackReader::open(&path).is_err());
        fs::write(&path, &original).unwrap();
        let mut reader = GenerationPackReader::open(&path).unwrap();
        let offset = reader.entries["a"].offset as usize;
        let mut corrupt = original;
        corrupt[offset] ^= 1;
        fs::write(&path, corrupt).unwrap();
        assert!(reader.read("a", 100).is_err());
    }

    #[test]
    fn directory_checksum_corruption_is_rejected_before_decode() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("generation.pack");
        let mut writer = GenerationPackWriter::new();
        writer.add("a", b"payload".to_vec()).unwrap();
        writer.publish(&path).unwrap();
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
        let mut writer = GenerationPackWriter::new();
        writer.add("stable", b"ok".to_vec()).unwrap();
        writer.publish(&path).unwrap();
        fs::write(path.with_extension("pack.tmp-crash"), b"crash").unwrap();
        let mut reader = GenerationPackReader::open(path).unwrap();
        assert_eq!(reader.read("stable", 2).unwrap().unwrap(), b"ok");
    }
}
