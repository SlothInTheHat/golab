//! Continuous indexing.
//!
//! The plan's Phase 4 line is "instead of parsing the repository every prompt,
//! maintain a continuously updated model". This is the part that makes the
//! model *stay* current: a filesystem watcher that re-indexes exactly the
//! files that changed, within a few hundred milliseconds of them changing.
//!
//! It runs on its own database connection so it can live in a background
//! thread beside the HTTP server, or on its own via `atlas index --watch`,
//! with no coordination between them beyond SQLite's.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use atlas_core::notify::scan_and_notify;
use atlas_core::scan::{self, ScanStats};
use atlas_core::Store;
use notify::{RecursiveMode, Watcher as _};

/// Editors save in bursts — write, rename, chmod. Wait for the burst to end
/// rather than re-indexing three times.
pub const DEBOUNCE: Duration = Duration::from_millis(250);

/// Stop signal for a watch loop.
#[derive(Clone, Default)]
pub struct Stop(Arc<AtomicBool>);

impl Stop {
    pub fn new() -> Stop {
        Stop::default()
    }
    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Watch `root` and re-index changed files until `stop` is set.
///
/// `on_index` is called once per debounced batch with the paths and the stats,
/// so a CLI can print and a daemon can stay quiet.
pub fn watch(
    root: &Path,
    stop: Stop,
    mut on_index: impl FnMut(&[PathBuf], &ScanStats),
) -> Result<()> {
    let mut store = Store::open(&atlas_core::db_path(root))?;
    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;

    let root = root.to_path_buf();
    loop {
        if stop.stopped() {
            return Ok(());
        }
        // Wake up regularly even when idle so `stop` is honoured promptly.
        let first = match rx.recv_timeout(Duration::from_millis(400)) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };

        let mut batch: BTreeSet<PathBuf> = BTreeSet::new();
        collect_paths(first, &root, &mut batch);

        // Drain the rest of the burst.
        let deadline = Instant::now() + DEBOUNCE;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(remaining) {
                Ok(event) => collect_paths(event, &root, &mut batch),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        if batch.is_empty() {
            continue;
        }

        let paths: Vec<PathBuf> = batch.into_iter().collect();
        // A deleted file cannot be indexed by path, and a deleted *manifest*
        // changes service membership, so fall back to a full scan when
        // anything vanished.
        let deleted = paths.iter().any(|p| !root.join(p).exists());
        let targets: &[PathBuf] = if deleted { &[] } else { &paths };
        // This is the live-editing path the proactive-notice feature exists
        // for, so it goes through `scan_and_notify` rather than plain `scan`.
        // The watcher follows the default repo only — see `atlas repo`.
        match scan_and_notify(&mut store, atlas_core::ids::DEFAULT_REPO_ID, &root, targets, false) {
            Ok(stats) => on_index(&paths, &stats),
            Err(e) => eprintln!("atlas: re-index failed: {e:#}"),
        }
    }
}

/// Turn a filesystem event into repo-relative paths worth re-indexing.
fn collect_paths(
    event: notify::Result<notify::Event>,
    root: &Path,
    out: &mut BTreeSet<PathBuf>,
) {
    let Ok(event) = event else { return };
    for path in event.paths {
        let Some(rel) = scan::relative_path(root, &path) else {
            continue;
        };
        if is_noise(&rel) {
            continue;
        }
        if scan::is_indexable(&rel) {
            out.insert(PathBuf::from(rel));
        }
    }
}

/// Directories whose churn is never interesting.
fn is_noise(rel: &str) -> bool {
    rel.starts_with(".atlas/")
        || rel.starts_with(".git/")
        || rel.contains("/node_modules/")
        || rel.starts_with("node_modules/")
        || rel.contains("/target/")
        || rel.starts_with("target/")
        || rel.contains("/__pycache__/")
        || rel.ends_with('~')
        || rel.ends_with(".tmp")
        || rel.ends_with(".swp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_and_build_noise_is_ignored() {
        for p in [
            ".atlas/runtime.db",
            ".git/index",
            "node_modules/react/index.js",
            "target/debug/x.rs",
            "src/pay.ts~",
            "src/.pay.ts.swp",
        ] {
            assert!(is_noise(p), "{p} should be ignored");
        }
        for p in ["src/pay.ts", "db/schema.sql", "Cargo.toml"] {
            assert!(!is_noise(p), "{p} should not be ignored");
        }
    }

    /// The whole point: touch a file, and the index reflects it without
    /// anyone running `atlas scan`.
    #[test]
    fn a_saved_file_is_reindexed_without_a_manual_scan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/pay.ts"), "export function charge() { return 1; }\n").unwrap();

        let mut store = Store::init(&root).unwrap();
        scan::scan(&mut store, atlas_core::ids::DEFAULT_REPO_ID, &root, &[], false).unwrap();
        assert!(store.resolve("refund").is_err(), "not written yet");
        drop(store);

        let stop = Stop::new();
        let (tx, rx) = channel::<()>();
        let handle = {
            let root = root.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                watch(&root, stop, |_paths, _stats| {
                    let _ = tx.send(());
                })
                .unwrap();
            })
        };

        // Give the watcher a moment to register before touching the tree.
        std::thread::sleep(Duration::from_millis(400));
        std::fs::write(
            root.join("src/pay.ts"),
            "export function charge() { return 1; }\nexport function refund() { return 2; }\n",
        )
        .unwrap();

        let indexed = rx.recv_timeout(Duration::from_secs(10)).is_ok();
        stop.stop();
        handle.join().unwrap();
        assert!(indexed, "the watcher should have re-indexed the change");

        let store = Store::open(&atlas_core::db_path(&root)).unwrap();
        assert!(
            store.resolve("refund").is_ok(),
            "the new symbol should be in the graph"
        );
    }
}
