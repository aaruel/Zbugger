//! The symbol index, in a compact form that's cheap to build, free, and cache.
//!
//! - Names, scopes and paths are interned in one [`StringTable`].
//! - Each file keeps its symbols as fixed-size [`SymbolRecord`]s.
//! - Lookup by name uses `postings`: one sorted array of references for all
//!   files, plus a small `overlay` for files updated since it was built (an
//!   edited file being re-indexed). Bulk indexing and loading the cache
//!   rebuild the array once at the end.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::strings::{StrId, StringTable};
use super::{scope_matches, Candidate, Reference, Symbol, SymbolKind};
use crate::document::path_key;

/// One symbol. 24 bytes; strings are ids into the index's string table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SymbolRecord {
    pub name: StrId,
    pub scope: StrId,
    /// Index into [`SymbolIndex::files`].
    pub file: u32,
    /// Zero-based line, and byte column within it.
    pub line: u32,
    pub column: u32,
    pub kind: SymbolKind,
    pub definition: bool,
}

/// A file's modification time and size, to tell whether a cached index of it
/// is still valid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileStamp {
    /// Nanoseconds since the Unix epoch.
    pub modified: u64,
    pub size: u64,
}

impl FileStamp {
    /// The file's current stamp; the default (all zero) if it can't be read,
    /// which never matches a real file, so it gets re-checked next time.
    pub fn of(path: &Path) -> Self {
        let Ok(meta) = std::fs::metadata(path) else { return Self::default() };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as u64);
        Self { modified, size: meta.len() }
    }
}

pub struct IndexedFile {
    /// The path as given, for display and opening.
    pub path: StrId,
    /// The path's [`path_key`], for matching the same file named differently.
    pub key: StrId,
    pub stamp: FileStamp,
    pub records: Vec<SymbolRecord>,
    /// Whether `postings` holds this file's current records.
    in_postings: bool,
    /// Dropped from the index (e.g. no longer in the project).
    removed: bool,
}

/// Points at `files[file].records[index]`.
#[derive(Clone, Copy, Debug)]
struct SymbolRef {
    file: u32,
    index: u32,
}

/// Rebuild `postings` once the overlay holds this many references.
const MAX_OVERLAY: usize = 100_000;

#[derive(Default)]
pub struct SymbolIndex {
    pub(super) strings: StringTable,
    pub(super) files: Vec<IndexedFile>,
    file_by_key: HashMap<StrId, u32>,
    /// References for all `in_postings` files, sorted by name.
    postings: Vec<SymbolRef>,
    /// Each name's range in `postings`.
    name_ranges: HashMap<StrId, (u32, u32)>,
    /// References for files updated since `postings` was built.
    overlay: HashMap<StrId, Vec<SymbolRef>>,
    overlay_len: usize,
}

/// Symbols extracted from one file, with string ids into a separate (per
/// worker) string table, ready to merge into an index.
pub struct ExtractedFile {
    pub path: PathBuf,
    pub stamp: FileStamp,
    pub records: Vec<SymbolRecord>,
}

impl SymbolIndex {
    /// Build an index from cached parts: (path id, key id, stamp, records) per file.
    pub(super) fn from_parts(
        strings: StringTable,
        files: Vec<(StrId, StrId, FileStamp, Vec<SymbolRecord>)>,
    ) -> Self {
        let mut index = Self { strings, ..Self::default() };
        for (path, key, stamp, mut records) in files {
            let id = index.files.len() as u32;
            for record in &mut records {
                record.file = id;
            }
            index.files.push(IndexedFile { path, key, stamp, records, in_postings: false, removed: false });
            index.file_by_key.insert(key, id);
        }
        index.rebuild_postings();
        index
    }

    /// Files currently in the index (not removed).
    pub(super) fn live_files(&self) -> impl Iterator<Item = &IndexedFile> {
        self.files.iter().filter(|f| !f.removed)
    }

    pub fn file_count(&self) -> usize {
        self.files.iter().filter(|f| !f.removed).count()
    }

    pub fn symbol_count(&self) -> usize {
        self.files.iter().map(|f| f.records.len()).sum()
    }

    fn key_of(path: &Path) -> String {
        path_key(path).to_string_lossy().into_owned()
    }

    fn file_id(&self, path: &Path) -> Option<u32> {
        let key = self.strings.find(&Self::key_of(path))?;
        self.file_by_key.get(&key).copied().filter(|&id| !self.files[id as usize].removed)
    }

    /// The stamp the index has for `path`, if it has indexed it.
    pub fn stamp_of(&self, path: &Path) -> Option<FileStamp> {
        self.file_id(path).map(|id| self.files[id as usize].stamp)
    }

    /// The id for `path`, adding an empty entry if it's new.
    fn file_entry(&mut self, path: &Path) -> u32 {
        let key = self.strings.intern(&Self::key_of(path));
        if let Some(&id) = self.file_by_key.get(&key) {
            return id;
        }
        let path = self.strings.intern(&path.to_string_lossy());
        let id = self.files.len() as u32;
        self.files.push(IndexedFile {
            path,
            key,
            stamp: FileStamp::default(),
            records: Vec::new(),
            in_postings: false,
            removed: false,
        });
        self.file_by_key.insert(key, id);
        id
    }

    /// Re-index one file from `text` (e.g. an open document with unsaved
    /// edits). Its stamp is left unknown, so the next refresh re-checks it.
    pub fn update_file_text(&mut self, parser: &mut tree_sitter::Parser, path: &Path, text: &str) {
        let mut strings = StringTable::default();
        let records = super::extract::extract(parser, text, &mut strings);
        self.replace_file(path, FileStamp::default(), &records, &strings);
    }

    /// Replace one file's symbols, e.g. after an edit. `records` use ids from
    /// `strings`. Lookups see the change straight away.
    pub fn replace_file(&mut self, path: &Path, stamp: FileStamp, records: &[SymbolRecord], strings: &StringTable) {
        let id = self.file_entry(path);
        self.unlink_overlay(id);
        let records = self.remap(records, strings, id);
        for (index, record) in records.iter().enumerate() {
            self.overlay.entry(record.name).or_default().push(SymbolRef { file: id, index: index as u32 });
        }
        self.overlay_len += records.len();
        let file = &mut self.files[id as usize];
        file.records = records;
        file.stamp = stamp;
        file.in_postings = false; // its old entries in `postings`, if any, are now ignored
        file.removed = false;
        if self.overlay_len > MAX_OVERLAY {
            self.rebuild_postings();
        }
    }

    /// Add many files at once (bulk indexing). Lookups are only correct after
    /// [`Self::rebuild_postings`].
    pub fn merge_bulk(&mut self, files: Vec<ExtractedFile>, strings: &StringTable) {
        let mut remap: Vec<Option<StrId>> = vec![None; strings.len()];
        for file in files {
            let id = self.file_entry(&file.path);
            let mut records = file.records;
            for record in &mut records {
                record.name = Self::remap_id(&mut self.strings, strings, &mut remap, record.name);
                record.scope = Self::remap_id(&mut self.strings, strings, &mut remap, record.scope);
                record.file = id;
            }
            let entry = &mut self.files[id as usize];
            entry.records = records;
            entry.stamp = file.stamp;
            entry.in_postings = false;
            entry.removed = false;
        }
    }

    /// Drop every file whose key isn't in `keep` (files no longer in the project).
    /// Returns how many were removed. Call [`Self::rebuild_postings`] afterwards.
    pub fn retain_files(&mut self, keep: &dyn Fn(&str) -> bool) -> usize {
        let mut removed = 0;
        for file in &mut self.files {
            if !file.removed && !keep(self.strings.get(file.key)) {
                file.removed = true;
                file.records = Vec::new();
                file.in_postings = false;
                removed += 1;
            }
        }
        removed
    }

    /// Rebuild the sorted name lookup from every file's records.
    pub fn rebuild_postings(&mut self) {
        zone!("rebuild symbol postings");
        let mut refs: Vec<(StrId, SymbolRef)> = Vec::with_capacity(self.symbol_count());
        for (file_id, file) in self.files.iter_mut().enumerate() {
            for (index, record) in file.records.iter().enumerate() {
                refs.push((record.name, SymbolRef { file: file_id as u32, index: index as u32 }));
            }
            file.in_postings = !file.removed;
        }
        refs.sort_unstable_by_key(|(name, r)| (*name, r.file, r.index));
        self.name_ranges.clear();
        let mut start = 0;
        while start < refs.len() {
            let name = refs[start].0;
            let end = start + refs[start..].iter().take_while(|(n, _)| *n == name).count();
            self.name_ranges.insert(name, (start as u32, (end - start) as u32));
            start = end;
        }
        self.postings = refs.into_iter().map(|(_, r)| r).collect();
        self.overlay.clear();
        self.overlay_len = 0;
    }

    /// Remove `file`'s current overlay references (before replacing its records).
    fn unlink_overlay(&mut self, file: u32) {
        let entry = &self.files[file as usize];
        if entry.in_postings || entry.records.is_empty() {
            return;
        }
        for record in &entry.records {
            if let Some(refs) = self.overlay.get_mut(&record.name) {
                let before = refs.len();
                refs.retain(|r| r.file != file);
                self.overlay_len -= before - refs.len();
            }
        }
    }

    fn remap(&mut self, records: &[SymbolRecord], strings: &StringTable, file: u32) -> Vec<SymbolRecord> {
        let mut remap = vec![None; strings.len()];
        records
            .iter()
            .map(|r| SymbolRecord {
                name: Self::remap_id(&mut self.strings, strings, &mut remap, r.name),
                scope: Self::remap_id(&mut self.strings, strings, &mut remap, r.scope),
                file,
                ..*r
            })
            .collect()
    }

    fn remap_id(into: &mut StringTable, from: &StringTable, remap: &mut [Option<StrId>], id: StrId) -> StrId {
        *remap[id as usize].get_or_insert_with(|| into.intern(from.get(id)))
    }

    /// Every current record named `name`.
    fn records_named(&self, name: StrId) -> impl Iterator<Item = &SymbolRecord> {
        let base = self.name_ranges.get(&name).map_or(&[][..], |&(start, len)| {
            &self.postings[start as usize..(start + len) as usize]
        });
        let base = base.iter().filter(|r| self.files[r.file as usize].in_postings);
        let overlay = self.overlay.get(&name).into_iter().flatten();
        base.chain(overlay).map(|r| &self.files[r.file as usize].records[r.index as usize])
    }

    fn resolve(&self, record: &SymbolRecord) -> Symbol {
        Symbol {
            name: self.strings.get(record.name).to_owned(),
            scope: self.strings.get(record.scope).to_owned(),
            kind: record.kind,
            definition: record.definition,
            path: PathBuf::from(self.strings.get(self.files[record.file as usize].path)),
            line: record.line as usize,
            column: record.column as usize,
        }
    }

    /// Everything indexed for `path`, in source order.
    #[cfg(test)]
    pub fn symbols_in_file(&self, path: &Path) -> Vec<Symbol> {
        let Some(id) = self.file_id(path) else { return Vec::new() };
        self.files[id as usize].records.iter().map(|r| self.resolve(r)).collect()
    }

    /// Candidates for `reference` (made in file `from`), best first.
    pub fn find(&self, reference: &Reference, from: &Path) -> Vec<Candidate> {
        let Some(name) = self.strings.find(&reference.name) else { return Vec::new() };
        let from_id = self.file_id(from);
        let from_stem = from.file_stem().map(|s| s.to_string_lossy().to_lowercase());
        let stem_of = |record: &SymbolRecord| {
            Path::new(self.strings.get(self.files[record.file as usize].path))
                .file_stem()
                .map(|s| s.to_string_lossy().to_lowercase())
        };

        let mut candidates: Vec<&SymbolRecord> = self.records_named(name).collect();
        // A qualifier like `Foo::` narrows the list, if anything matches it.
        if let Some(qualifier) = &reference.qualifier {
            let matching: Vec<&SymbolRecord> =
                candidates.iter().copied().filter(|r| scope_matches(self.strings.get(r.scope), qualifier)).collect();
            if !matching.is_empty() {
                candidates = matching;
            }
        }

        let score = |r: &SymbolRecord| -> i32 {
            let mut score = 0;
            if r.definition {
                score += 100;
            }
            if Some(r.file) == from_id {
                score += 30;
            } else if from_stem.is_some() && stem_of(r) == from_stem {
                score += 20; // foo.h <-> foo.cpp
            }
            let in_scope = !self.strings.get(r.scope).is_empty();
            if reference.call && matches!(r.kind, SymbolKind::Function | SymbolKind::Macro) {
                score += 10;
            }
            if reference.call && r.kind.is_type() {
                score += 5; // constructor call: Foo(...)
            }
            if reference.member && matches!(r.kind, SymbolKind::Function | SymbolKind::Field) && in_scope {
                score += 10;
            }
            if !reference.member && !reference.call && r.kind.is_type() {
                score += 5;
            }
            score
        };

        let mut ranked: Vec<(i32, &SymbolRecord)> = candidates.into_iter().map(|r| (score(r), r)).collect();
        let path_of = |r: &SymbolRecord| self.strings.get(self.files[r.file as usize].path);
        ranked.sort_by(|(a_score, a), (b_score, b)| {
            b_score.cmp(a_score).then_with(|| path_of(a).cmp(path_of(b))).then(a.line.cmp(&b.line))
        });
        ranked.dedup_by(|(_, a), (_, b)| a.file == b.file && a.line == b.line && a.column == b.column);
        ranked.into_iter().map(|(score, r)| Candidate { symbol: self.resolve(r), score }).collect()
    }
}
