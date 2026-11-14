//! Bringing a project's symbol index up to date on a background thread,
//! reusing the on-disk cache so only new or changed files are parsed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;

use rayon::prelude::*;

use super::extract::extract_files;
use super::index::{FileStamp, SymbolIndex};
use super::cache;
use crate::document::path_key;

/// What the indexer is doing, for the status bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    LoadingCache,
    CheckingFiles,
    Parsing,
}

/// Shared progress counters, updated by the worker threads.
#[derive(Default)]
pub struct Progress {
    phase: AtomicU8,
    done: AtomicUsize,
    total: AtomicUsize,
}

impl Progress {
    fn set(&self, phase: Phase, total: usize) {
        self.phase.store(phase as u8, Ordering::Relaxed);
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    fn tick(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get(&self) -> (Phase, usize, usize) {
        let phase = match self.phase.load(Ordering::Relaxed) {
            0 => Phase::LoadingCache,
            1 => Phase::CheckingFiles,
            _ => Phase::Parsing,
        };
        (phase, self.done.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed))
    }
}

/// What a refresh did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RefreshStats {
    pub from_cache: usize,
    pub parsed: usize,
    pub removed: usize,
}

/// Bring an index of `files` up to date: start from the cache at
/// `cache_file` (if any and valid), parse files that are new or whose
/// modification time or size changed, and drop files no longer listed.
/// Returns the index, what changed, and the bytes to write back to the cache
/// if anything did.
pub fn refresh(files: &[PathBuf], cache_file: Option<&Path>, progress: &Progress) -> (SymbolIndex, RefreshStats, Option<Vec<u8>>) {
    progress.set(Phase::LoadingCache, 0);
    let cached = cache_file.and_then(cache::read);
    let had_cache = cached.is_some();
    let mut index = cached.unwrap_or_default();

    // Which files changed since they were cached?
    progress.set(Phase::CheckingFiles, files.len());
    let stale: Vec<PathBuf> = {
        zone!("check file stamps");
        files
            .par_iter()
            .filter(|path| {
                let changed = index.stamp_of(path) != Some(FileStamp::of(path));
                progress.tick();
                changed
            })
            .cloned()
            .collect()
    };

    // Files that left the project.
    let keep: HashSet<String> = files.iter().map(|p| path_key(p).to_string_lossy().into_owned()).collect();
    let removed = index.retain_files(&|key| keep.contains(key));

    progress.set(Phase::Parsing, stale.len());
    for chunk in extract_files(&stale, &|| progress.tick()) {
        index.merge_bulk(chunk.files, &chunk.strings);
    }
    index.rebuild_postings();

    let stats = RefreshStats { from_cache: files.len() - stale.len(), parsed: stale.len(), removed };
    let changed = !had_cache || stats.parsed > 0 || stats.removed > 0;
    let save = (changed && cache_file.is_some()).then(|| cache::serialize(&index));
    (index, stats, save)
}

/// A running background refresh of a project's index.
pub struct Indexer {
    receiver: Receiver<(SymbolIndex, RefreshStats)>,
    progress: Arc<Progress>,
    /// The thread; after handing over the index it may still be writing the cache.
    thread: std::thread::JoinHandle<()>,
}

impl Indexer {
    /// Start indexing `files`, using and updating the cache at `cache_file`.
    /// `ctx` is asked to repaint when the index is ready.
    pub fn start(files: Vec<PathBuf>, cache_file: Option<PathBuf>, ctx: egui::Context) -> Self {
        let (sender, receiver) = mpsc::channel();
        let progress = Arc::new(Progress::default());
        let worker_progress = progress.clone();
        let thread = std::thread::Builder::new()
            .name("symbol-indexer".into())
            .spawn(move || {
                let (index, stats, save) = refresh(&files, cache_file.as_deref(), &worker_progress);
                // Hand the index over first; writing the cache can happen after.
                // (The receiver is gone if the project was closed meanwhile; that's fine.)
                let _ = sender.send((index, stats));
                ctx.request_repaint();
                if let (Some(path), Some(bytes)) = (cache_file, save) {
                    if let Err(err) = cache::write(&path, &bytes) {
                        eprintln!("can't write symbol cache {}: {err}", path.display());
                    }
                }
            })
            .expect("can spawn the indexing thread");
        Self { receiver, progress, thread }
    }

    pub fn progress(&self) -> (Phase, usize, usize) {
        self.progress.get()
    }

    /// The worker thread, to wait for its cache write before the process
    /// exits (exiting mid-write would lose the cache).
    pub fn into_thread(self) -> std::thread::JoinHandle<()> {
        self.thread
    }

    /// The finished index, once ready. `Err` means the thread died.
    pub fn try_take(&self) -> Option<Result<(SymbolIndex, RefreshStats), ()>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(Ok(result)),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(())),
        }
    }
}
