//! The on-disk symbol index cache: the compact index written out as-is.
//!
//! ```text
//! "ZBIX" magic, u64 version
//! u64 string byte count, the bytes, u32 span count, (u32 start, u32 len) per string
//! u32 file count, per file: u32 path id, u32 key id, u64 modified, u64 size, u32 record count
//! records, file by file: u32 name, u32 scope, u32 line, u32 column, u8 kind, u8 definition
//! ```
//!
//! Everything is little-endian. Anything inconsistent makes the whole cache
//! invalid, which just means re-indexing.

use std::path::{Path, PathBuf};

use super::extract::QUERY_SOURCE;
use super::index::{FileStamp, SymbolIndex, SymbolRecord};
use super::strings::StringTable;
use super::SymbolKind;
use crate::document::path_key;

const MAGIC: &[u8; 4] = b"ZBIX";
/// Bump when the layout or the extractor's behaviour changes.
const FORMAT_VERSION: u64 = 1;

/// Changes whenever the format or the symbol query changes, invalidating old caches.
fn version() -> u64 {
    fnv1a(QUERY_SOURCE.as_bytes()) ^ FORMAT_VERSION
}

/// A small, stable hash (unlike std's, which may change between Rust versions).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| (hash ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

/// Where the cache for a project or solution lives:
/// `%LOCALAPPDATA%\Zbugger\index\<name>-<hash of its path>.zbix`.
pub fn cache_path(project: &Path) -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let name = project.file_stem()?.to_string_lossy();
    let hash = fnv1a(path_key(project).to_string_lossy().as_bytes());
    Some(base.join("Zbugger").join("index").join(format!("{name}-{hash:016x}.zbix")))
}

pub fn serialize(index: &SymbolIndex) -> Vec<u8> {
    zone!("serialize symbol cache");
    let (bytes, spans) = index.strings.parts();
    let files: Vec<_> = index.live_files().collect();
    let records: usize = files.iter().map(|f| f.records.len()).sum();
    let mut out = Vec::with_capacity(64 + bytes.len() + spans.len() * 8 + files.len() * 28 + records * 18);

    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&version().to_le_bytes());
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes.as_bytes());
    out.extend_from_slice(&(spans.len() as u32).to_le_bytes());
    for &(start, len) in spans {
        out.extend_from_slice(&start.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
    }
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for file in &files {
        out.extend_from_slice(&file.path.to_le_bytes());
        out.extend_from_slice(&file.key.to_le_bytes());
        out.extend_from_slice(&file.stamp.modified.to_le_bytes());
        out.extend_from_slice(&file.stamp.size.to_le_bytes());
        out.extend_from_slice(&(file.records.len() as u32).to_le_bytes());
    }
    for file in &files {
        for r in &file.records {
            out.extend_from_slice(&r.name.to_le_bytes());
            out.extend_from_slice(&r.scope.to_le_bytes());
            out.extend_from_slice(&r.line.to_le_bytes());
            out.extend_from_slice(&r.column.to_le_bytes());
            out.push(r.kind as u8);
            out.push(r.definition as u8);
        }
    }
    out
}

/// Parse a cache; `None` if it's from another version or damaged.
pub fn deserialize(data: &[u8]) -> Option<SymbolIndex> {
    zone!("load symbol cache");
    let mut r = Reader { data, pos: 0 };
    if r.take(4)? != MAGIC || r.u64()? != version() {
        return None;
    }
    let byte_len = usize::try_from(r.u64()?).ok()?;
    let bytes = std::str::from_utf8(r.take(byte_len)?).ok()?.to_owned();
    let span_count = r.u32()? as usize;
    let mut spans = Vec::with_capacity(span_count.min(data.len() / 8));
    for _ in 0..span_count {
        spans.push((r.u32()?, r.u32()?));
    }
    let strings = StringTable::from_parts(bytes, spans)?;
    let string_count = strings.len() as u32;

    let file_count = r.u32()? as usize;
    let mut headers = Vec::with_capacity(file_count.min(data.len() / 28));
    for _ in 0..file_count {
        let (path, key) = (r.u32()?, r.u32()?);
        let stamp = FileStamp { modified: r.u64()?, size: r.u64()? };
        let count = r.u32()? as usize;
        if path >= string_count || key >= string_count {
            return None;
        }
        headers.push((path, key, stamp, count));
    }
    let mut files = Vec::with_capacity(headers.len());
    for (path, key, stamp, count) in headers {
        let mut records = Vec::with_capacity(count.min(data.len() / 18));
        for _ in 0..count {
            let (name, scope, line, column) = (r.u32()?, r.u32()?, r.u32()?, r.u32()?);
            let kind = SymbolKind::from_u8(r.u8()?)?;
            let definition = r.u8()? != 0;
            if name >= string_count || scope >= string_count {
                return None;
            }
            records.push(SymbolRecord { name, scope, file: 0, line, column, kind, definition });
        }
        files.push((path, key, stamp, records));
    }
    if r.pos != data.len() {
        return None; // trailing junk: written by something else
    }
    Some(SymbolIndex::from_parts(strings, files))
}

pub fn read(path: &Path) -> Option<SymbolIndex> {
    deserialize(&std::fs::read(path).ok()?)
}

/// Write via a temporary file and rename, so quitting mid-write can't leave
/// a half-written cache behind.
pub fn write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    zone!("write symbol cache");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension("zbix.tmp");
    std::fs::write(&temp, data)?;
    std::fs::rename(&temp, path)
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        let slice = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}
