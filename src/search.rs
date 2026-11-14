//! Find in Files: searching every file in the solution, in parallel, with
//! results streamed back as they're found.
//!
//! It's a brute-force scan like ripgrep: every file is read and searched with
//! the `regex` crate (which uses fast literal prefilters even for plain text).
//! Files open in the editor are searched from memory, so unsaved edits count.

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Instant;

use rayon::prelude::*;
use regex::bytes::{Regex, RegexBuilder};

/// Stop after this many matches, so a very common word stays responsive.
pub const MAX_MATCHES: usize = 10_000;
/// Longest line excerpt shown for a match, in bytes.
const MAX_EXCERPT: usize = 240;
/// Skip files larger than this (generated data, logs...).
const MAX_FILE_SIZE: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

impl SearchQuery {
    /// The query as a regex. Plain text is escaped; whole word wraps it in `\b`.
    pub fn compile(&self) -> Result<Regex, String> {
        if self.text.is_empty() {
            return Err("empty search".to_owned());
        }
        let mut pattern = if self.regex { self.text.clone() } else { regex::escape(&self.text) };
        if self.whole_word {
            pattern = format!(r"\b(?:{pattern})\b");
        }
        RegexBuilder::new(&pattern)
            .case_insensitive(!self.case_sensitive)
            .multi_line(true)
            .build()
            .map_err(|e| match e {
                regex::Error::Syntax(message) => message.lines().last().unwrap_or("invalid pattern").trim().to_owned(),
                other => other.to_string(),
            })
    }
}

/// One match, with the line it's on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMatch {
    /// Zero-based line, and byte column of the match in it.
    pub line: usize,
    pub column: usize,
    /// Length of the match in bytes.
    pub len: usize,
    /// The line (or a window of it around the match, if it's long), trimmed.
    pub excerpt: String,
    /// The match's position within `excerpt`.
    pub highlight: Range<usize>,
}

#[derive(Clone, Debug)]
pub struct FileMatches {
    pub path: PathBuf,
    pub matches: Vec<LineMatch>,
}

/// Search one file's contents. At most `limit` matches are returned.
/// Binary files (a NUL byte near the start) give no matches.
pub fn search_bytes(regex: &Regex, bytes: &[u8], limit: usize) -> Vec<LineMatch> {
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Vec::new();
    }
    let mut matches = Vec::new();
    // Line counting continues from the previous match.
    let (mut counted_to, mut line) = (0, 0);
    for found in regex.find_iter(bytes) {
        if matches.len() >= limit {
            break;
        }
        if found.is_empty() {
            continue; // patterns like `x*` match everywhere; nothing to show
        }
        line += memchr::memchr_iter(b'\n', &bytes[counted_to..found.start()]).count();
        counted_to = found.start();
        let line_start = memchr::memrchr(b'\n', &bytes[..found.start()]).map_or(0, |i| i + 1);
        let line_end = memchr::memchr(b'\n', &bytes[found.start()..]).map_or(bytes.len(), |i| found.start() + i);
        let line_end = if line_end > line_start && bytes[line_end - 1] == b'\r' { line_end - 1 } else { line_end };
        let column = found.start() - line_start;
        let (excerpt, highlight) =
            excerpt(&bytes[line_start..line_end], column..(found.end().min(line_end) - line_start));
        matches.push(LineMatch { line, column, len: found.len(), excerpt, highlight });
    }
    matches
}

/// The line, trimmed and cut to a window around `hit` if it's long; and
/// where `hit` ends up in the result.
fn excerpt(line: &[u8], hit: Range<usize>) -> (String, Range<usize>) {
    let mut start = line.iter().take_while(|c| c.is_ascii_whitespace()).count().min(hit.start);
    let mut end = line.len();
    if end - start > MAX_EXCERPT {
        start = hit.start.saturating_sub(MAX_EXCERPT / 3).max(start);
        end = (start + MAX_EXCERPT).min(line.len()).max(hit.end);
    }
    // Keep UTF-8 boundaries (lossy conversion would shift offsets otherwise).
    let text = String::from_utf8_lossy(&line[start..end]);
    if text.len() == end - start {
        (text.into_owned(), hit.start - start..hit.end - start)
    } else {
        // Invalid UTF-8 changed lengths; highlight approximately.
        let text = text.into_owned();
        let h = (hit.start - start).min(text.len());
        (text.clone(), h..(h + hit.len()).min(text.len()))
    }
}

/// A search running in the background.
pub struct Search {
    receiver: Receiver<FileMatches>,
    cancel: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    searched: Arc<AtomicUsize>,
    pub total_files: usize,
    started: Instant,
    /// When the search finished, measured from `started`.
    elapsed: Option<std::time::Duration>,
}

impl Search {
    /// Start searching `files`, using `open` texts (path key → text) instead
    /// of the disk for files open in the editor. `ctx` is asked to repaint as
    /// results arrive.
    pub fn start(
        query: SearchQuery,
        files: Arc<Vec<PathBuf>>,
        open: HashMap<PathBuf, String>,
        ctx: egui::Context,
    ) -> Result<Self, String> {
        let regex = query.compile()?;
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let searched = Arc::new(AtomicUsize::new(0));
        let total_files = files.len();
        let (thread_cancel, thread_finished, thread_searched) = (cancel.clone(), finished.clone(), searched.clone());
        std::thread::Builder::new()
            .name("find-in-files".into())
            .spawn(move || {
                zone!("find in files");
                let found = AtomicUsize::new(0);
                files.par_iter().for_each_with(sender, |sender, path| {
                    if thread_cancel.load(Ordering::Relaxed) || found.load(Ordering::Relaxed) >= MAX_MATCHES {
                        return;
                    }
                    let key = crate::document::path_key(path);
                    let limit = MAX_MATCHES - found.load(Ordering::Relaxed).min(MAX_MATCHES);
                    let matches = match open.get(&key) {
                        Some(text) => search_bytes(&regex, text.as_bytes(), limit),
                        None => match std::fs::metadata(path) {
                            Ok(meta) if meta.len() <= MAX_FILE_SIZE => match std::fs::read(path) {
                                Ok(bytes) => search_bytes(&regex, &bytes, limit),
                                Err(_) => Vec::new(),
                            },
                            _ => Vec::new(),
                        },
                    };
                    thread_searched.fetch_add(1, Ordering::Relaxed);
                    if !matches.is_empty() {
                        found.fetch_add(matches.len(), Ordering::Relaxed);
                        let _ = sender.send(FileMatches { path: path.clone(), matches });
                    }
                });
                thread_finished.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            })
            .expect("can spawn the search thread");
        Ok(Self { receiver, cancel, finished, searched, total_files, started: Instant::now(), elapsed: None })
    }

    /// Results found since the last call.
    pub fn take_results(&mut self) -> Vec<FileMatches> {
        let results: Vec<FileMatches> = self.receiver.try_iter().collect();
        if self.elapsed.is_none() && self.finished.load(Ordering::Relaxed) {
            // Collect anything sent just before finishing.
            let mut results = results;
            results.extend(self.receiver.try_iter());
            self.elapsed = Some(self.started.elapsed());
            return results;
        }
        results
    }

    pub fn is_finished(&self) -> bool {
        self.elapsed.is_some()
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.elapsed.unwrap_or_else(|| self.started.elapsed())
    }

    pub fn files_searched(&self) -> usize {
        self.searched.load(Ordering::Relaxed)
    }
}

impl Drop for Search {
    /// A replaced or closed search stops as soon as its workers notice.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(text: &str) -> SearchQuery {
        SearchQuery { text: text.to_owned(), ..SearchQuery::default() }
    }

    fn found(q: &SearchQuery, text: &str) -> Vec<(usize, usize, String, String)> {
        search_bytes(&q.compile().unwrap(), text.as_bytes(), usize::MAX)
            .into_iter()
            .map(|m| {
                let hit = m.excerpt[m.highlight.clone()].to_owned();
                (m.line, m.column, m.excerpt, hit)
            })
            .collect()
    }

    #[test]
    fn finds_lines_and_columns() {
        let text = "int Health = 100;\n    float health;\r\nvoid Heal();\n";
        assert_eq!(
            found(&query("health"), text),
            [
                (0, 4, "int Health = 100;".to_owned(), "Health".to_owned()),
                (1, 10, "float health;".to_owned(), "health".to_owned()),
            ]
        );
        let case = SearchQuery { case_sensitive: true, ..query("Health") };
        assert_eq!(found(&case, text).len(), 1);
    }

    #[test]
    fn whole_words_and_regex() {
        let text = "Heal Healing heal\n";
        let word = SearchQuery { whole_word: true, ..query("heal") };
        assert_eq!(found(&word, text).iter().map(|m| m.1).collect::<Vec<_>>(), [0, 13]);
        let re = SearchQuery { regex: true, ..query(r"heal\w+") };
        assert_eq!(found(&re, text).iter().map(|m| m.3.as_str()).collect::<Vec<_>>(), ["Healing"]);
        // Regex characters are literal in plain-text mode.
        assert_eq!(found(&query("a.b"), "axb a.b").iter().map(|m| m.1).collect::<Vec<_>>(), [4]);
        assert!(SearchQuery { regex: true, ..query("(unclosed") }.compile().is_err());
        assert!(query("").compile().is_err());
    }

    #[test]
    fn long_lines_are_cut_around_the_match() {
        let line = format!("{}needle{}", "x".repeat(1000), "y".repeat(1000));
        let m = &search_bytes(&query("needle").compile().unwrap(), line.as_bytes(), 10)[0];
        assert!(m.excerpt.len() <= MAX_EXCERPT + 6);
        assert_eq!(&m.excerpt[m.highlight.clone()], "needle");
        assert_eq!(m.column, 1000);
    }

    #[test]
    fn skips_binary_files_and_respects_the_limit() {
        assert!(search_bytes(&query("a").compile().unwrap(), b"a\0a", 10).is_empty());
        assert_eq!(search_bytes(&query("a").compile().unwrap(), b"aaaaa", 3).len(), 3);
    }

    /// Time searches over a real solution's files:
    /// `ZBUGGER_SLN=x.sln cargo test --release -- --ignored --nocapture search_timing`
    #[test]
    #[ignore]
    fn search_timing() {
        let solution = std::env::var("ZBUGGER_SLN").expect("set ZBUGGER_SLN");
        let view = crate::project_view::ProjectView::open(std::path::Path::new(&solution)).unwrap();
        let files = view.search_files();
        let bytes: u64 = files.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
        println!("{} files, {:.0} MB", files.len(), bytes as f64 / 1e6);
        let cases = [
            ("GetActorLocation", false, false),
            ("UPROPERTY", false, false),
            ("Tick", true, false),
            (r"class \w+_API AActor\b", false, true),
            ("zzqqxx_no_such_text", false, false),
        ];
        for (text, whole_word, regex) in cases {
            let query = SearchQuery { text: text.to_owned(), whole_word, regex, ..SearchQuery::default() };
            let mut search = Search::start(query, files.clone(), HashMap::new(), egui::Context::default()).unwrap();
            let (mut first, mut matches, mut in_files) = (None, 0, 0);
            loop {
                let results = search.take_results();
                if first.is_none() && !results.is_empty() {
                    first = Some(search.elapsed());
                }
                in_files += results.len();
                matches += results.iter().map(|f| f.matches.len()).sum::<usize>();
                if search.is_finished() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            println!(
                "{text:?}{}{}: {matches} matches in {in_files} files, first result {:?}, done in {:?} ({} files searched)",
                if whole_word { " (word)" } else { "" },
                if regex { " (regex)" } else { "" },
                first.unwrap_or_default(),
                search.elapsed(),
                search.files_searched()
            );
        }
    }

    #[test]
    fn searches_files_and_open_documents_in_the_background() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.cpp");
        let b = dir.path().join("b.h");
        std::fs::write(&a, "// target here\n").unwrap();
        std::fs::write(&b, "nothing\n").unwrap();
        // b.h is open with an unsaved edit that matches.
        let open = HashMap::from([(crate::document::path_key(&b), "unsaved target\n".to_owned())]);
        let mut search =
            Search::start(query("target"), Arc::new(vec![a.clone(), b.clone()]), open, egui::Context::default())
                .unwrap();
        let mut results = Vec::new();
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !search.is_finished() && Instant::now() < deadline {
            results.extend(search.take_results());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        results.extend(search.take_results());
        results.sort_by(|x, y| x.path.cmp(&y.path));
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].path, a);
        assert_eq!(results[1].matches[0].excerpt, "unsaved target");
        assert_eq!(search.files_searched(), 2);
    }
}
