//! Interned strings: every distinct string is stored once in one buffer and
//! referred to by a 4-byte id.
//!
//! Symbol names and scopes repeat constantly across a codebase (`FVector`,
//! `UObject`, `Engine::Runtime`...), so this is far smaller than one `String`
//! per symbol, and it's a handful of large allocations instead of millions of
//! small ones, which makes building, freeing and caching the index cheap.

use std::hash::BuildHasher;

use hashbrown::HashTable;

pub type StrId = u32;

#[derive(Default)]
pub struct StringTable {
    /// All strings, back to back.
    bytes: String,
    /// (start, length) in `bytes` of each id.
    spans: Vec<(u32, u32)>,
    /// Ids, found by hashing the string they name.
    lookup: HashTable<StrId>,
    hasher: hashbrown::DefaultHashBuilder,
}

impl StringTable {
    pub fn get(&self, id: StrId) -> &str {
        let (start, len) = self.spans[id as usize];
        &self.bytes[start as usize..(start + len) as usize]
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// The id of `text`, adding it if new.
    pub fn intern(&mut self, text: &str) -> StrId {
        let hash = self.hasher.hash_one(text);
        let (bytes, spans) = (&self.bytes, &self.spans);
        let text_of = |id: StrId| {
            let (start, len) = spans[id as usize];
            &bytes[start as usize..(start + len) as usize]
        };
        if let Some(&id) = self.lookup.find(hash, |&id| text_of(id) == text) {
            return id;
        }
        let id = self.spans.len() as StrId;
        self.spans.push((self.bytes.len() as u32, text.len() as u32));
        self.bytes.push_str(text);
        let (bytes, spans, hasher) = (&self.bytes, &self.spans, &self.hasher);
        self.lookup.insert_unique(hash, id, |&id| {
            let (start, len) = spans[id as usize];
            hasher.hash_one(&bytes[start as usize..(start + len) as usize])
        });
        id
    }

    /// The id of `text`, if it's in the table.
    pub fn find(&self, text: &str) -> Option<StrId> {
        let hash = self.hasher.hash_one(text);
        self.lookup.find(hash, |&id| self.get(id) == text).copied()
    }

    /// The raw parts, for writing to the cache.
    pub fn parts(&self) -> (&str, &[(u32, u32)]) {
        (&self.bytes, &self.spans)
    }

    /// Rebuild a table from cached parts. Returns `None` if they're inconsistent.
    pub fn from_parts(bytes: String, spans: Vec<(u32, u32)>) -> Option<Self> {
        let mut table = Self { bytes, spans: Vec::new(), lookup: HashTable::with_capacity(spans.len()), hasher: Default::default() };
        for &(start, len) in &spans {
            let end = start.checked_add(len)? as usize;
            if end > table.bytes.len() || !table.bytes.is_char_boundary(start as usize) || !table.bytes.is_char_boundary(end) {
                return None;
            }
        }
        table.spans = spans;
        let (bytes, spans, hasher) = (&table.bytes, &table.spans, &table.hasher);
        let text_of = |id: StrId| {
            let (start, len) = spans[id as usize];
            &bytes[start as usize..(start + len) as usize]
        };
        for id in 0..spans.len() as StrId {
            table.lookup.insert_unique(hasher.hash_one(text_of(id)), id, |&id| hasher.hash_one(text_of(id)));
        }
        Some(table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_once() {
        let mut table = StringTable::default();
        let a = table.intern("FVector");
        let b = table.intern("UObject");
        assert_eq!(table.intern("FVector"), a);
        assert_ne!(a, b);
        assert_eq!(table.get(a), "FVector");
        assert_eq!(table.get(b), "UObject");
        assert_eq!(table.find("UObject"), Some(b));
        assert_eq!(table.find("missing"), None);
        assert_eq!(table.intern(""), 2);
        assert_eq!(table.get(2), "");
        assert_eq!(table.len(), 3);
    }

    #[test]
    fn grows_past_many_entries() {
        let mut table = StringTable::default();
        let ids: Vec<_> = (0..10_000).map(|i| table.intern(&format!("name{i}"))).collect();
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(table.get(*id), format!("name{i}"));
            assert_eq!(table.find(&format!("name{i}")), Some(*id));
        }
    }

    #[test]
    fn round_trips_through_parts() {
        let mut table = StringTable::default();
        for word in ["alpha", "βeta", "gamma"] {
            table.intern(word);
        }
        let (bytes, spans) = table.parts();
        let copy = StringTable::from_parts(bytes.to_owned(), spans.to_vec()).unwrap();
        assert_eq!(copy.find("βeta"), Some(1));
        assert_eq!(copy.get(2), "gamma");
        // Spans pointing outside the bytes, or into the middle of a character, are rejected.
        assert!(StringTable::from_parts("abc".into(), vec![(1, 5)]).is_none());
        assert!(StringTable::from_parts("βx".into(), vec![(1, 1)]).is_none());
    }
}
