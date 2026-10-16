//! Write-ahead file checkpoints: every file a write tool is about to touch is
//! copied aside first, grouped per task run ("epoch"). `/undo` restores the
//! newest epoch — an edit-safety net for agent-driven changes.
//!
//! Layout (under the platform data dir, `kazi/checkpoints/`):
//! ```text
//! <root>/<epoch>/manifest.jsonl   one JSON entry per snapshot, append order
//! <root>/<epoch>/files/<idx>      the pre-write copy of each file
//! ```
//! Entries record `existed: false` for to-be-created files, so undo deletes
//! them. Undo replays entries in reverse (last snapshot wins per file) and is
//! best-effort per file: one failure does not abort the rest.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// How many epochs to keep on disk.
const KEEP_EPOCHS: usize = 20;

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    /// Absolute path that was (about to be) written.
    path: PathBuf,
    /// Copy of the original content, relative to the epoch dir. `None` when
    /// the file did not exist yet.
    backup: Option<PathBuf>,
    existed: bool,
}

/// Low-level store: epoch allocation, snapshots, undo.
pub struct Checkpoints {
    root: PathBuf,
}

impl Checkpoints {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Allocate the next epoch (`max-on-disk + 1`) and prune old ones.
    pub fn begin_epoch(&self) -> u64 {
        let n = self.latest_epoch().unwrap_or(0) + 1;
        let _ = std::fs::create_dir_all(self.root.join(n.to_string()));
        self.prune();
        n
    }

    /// Highest epoch present on disk, if any.
    pub fn latest_epoch(&self) -> Option<u64> {
        let mut best: Option<u64> = None;
        for e in std::fs::read_dir(&self.root).ok()?.flatten() {
            if let Ok(n) = e.file_name().to_string_lossy().parse::<u64>() {
                best = Some(best.map_or(n, |b: u64| b.max(n)));
            }
        }
        best
    }

    fn epoch_dir(&self, epoch: u64) -> PathBuf {
        self.root.join(epoch.to_string())
    }

    /// Copy `path` aside before it is modified. Best-effort: failures are
    /// swallowed (an un-undoable write is worse than no safety net, but must
    /// never block the write itself).
    pub fn snapshot(&self, epoch: u64, path: &Path) {
        let dir = self.epoch_dir(epoch);
        let files = dir.join("files");
        if std::fs::create_dir_all(&files).is_err() {
            return;
        }
        let manifest = dir.join("manifest.jsonl");
        let idx = std::fs::read_to_string(&manifest)
            .map(|s| s.lines().count())
            .unwrap_or(0);

        let (backup, existed) = if path.exists() {
            let rel = PathBuf::from("files").join(idx.to_string());
            if std::fs::copy(path, dir.join(&rel)).is_err() {
                return;
            }
            (Some(rel), true)
        } else {
            (None, false)
        };

        let entry = Entry {
            path: path.to_path_buf(),
            backup,
            existed,
        };
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&manifest)
        {
            let _ = writeln!(f, "{}", serde_json::to_string(&entry).unwrap_or_default());
        }
    }

    /// Restore the newest epoch. Returns how many files were restored/removed.
    pub fn undo_latest(&self) -> std::io::Result<Option<usize>> {
        let Some(epoch) = self.latest_epoch() else {
            return Ok(None);
        };
        self.undo(epoch).map(Some)
    }

    /// Restore one epoch: entries replayed last-first so the newest snapshot
    /// of a repeatedly-written file wins.
    pub fn undo(&self, epoch: u64) -> std::io::Result<usize> {
        let dir = self.epoch_dir(epoch);
        let manifest = dir.join("manifest.jsonl");
        let text = std::fs::read_to_string(&manifest)?;
        let entries: Vec<Entry> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        let mut restored = 0usize;
        for entry in entries.iter().rev() {
            let ok = match (&entry.backup, entry.existed) {
                (Some(rel), true) => std::fs::copy(dir.join(rel), &entry.path).is_ok(),
                (None, false) => !entry.path.exists() || std::fs::remove_file(&entry.path).is_ok(),
                (Some(_), false) => true, // inconsistent manifest: skip
                (None, true) => false,
            };
            if ok {
                restored += 1;
            }
        }
        Ok(restored)
    }

    fn prune(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        let mut epochs: Vec<(u64, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_string_lossy()
                    .parse::<u64>()
                    .ok()
                    .map(|n| (n, e.path()))
            })
            .collect();
        if epochs.len() <= KEEP_EPOCHS {
            return;
        }
        epochs.sort_by_key(|(n, _)| *n);
        for (_, path) in epochs.iter().take(epochs.len() - KEEP_EPOCHS) {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// Task-scoped handle held by [`super::ToolCtx`]: remembers which epoch the
/// current run is writing into, so tools can snapshot without arguments.
pub struct Checkpointing {
    store: Checkpoints,
    epoch: Mutex<Option<u64>>,
}

impl Checkpointing {
    pub fn new(root: PathBuf) -> Self {
        Self {
            store: Checkpoints::new(root),
            epoch: Mutex::new(None),
        }
    }

    /// Open a new epoch for the task about to run.
    pub fn begin_epoch(&self) -> u64 {
        let n = self.store.begin_epoch();
        *self.epoch.lock().unwrap_or_else(|e| e.into_inner()) = Some(n);
        n
    }

    /// Snapshot a path into the current epoch (no-op outside a run).
    pub fn snapshot(&self, path: &Path) {
        let epoch = *self.epoch.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(epoch) = epoch {
            self.store.snapshot(epoch, path);
        }
    }

    /// `/undo`: restore the newest epoch.
    pub fn undo_latest(&self) -> std::io::Result<Option<usize>> {
        self.store.undo_latest()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let id = std::process::id() as u64
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;
        let d = std::env::temp_dir().join(format!("kazi-ckpt-{name}-{id}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn undo_restores_modified_files() {
        let root = tempdir("restore");
        let cp = Checkpointing::new(root.join("store"));
        let file = root.join("note.txt");
        std::fs::write(&file, "original").unwrap();

        let _epoch = cp.begin_epoch();
        cp.snapshot(&file);
        std::fs::write(&file, "changed by the agent").unwrap();

        let restored = cp.undo_latest().unwrap().expect("epoch exists");
        assert_eq!(restored, 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_deletes_files_that_did_not_exist() {
        let root = tempdir("delete");
        let cp = Checkpointing::new(root.join("store"));
        let file = root.join("brand-new.txt");

        let _epoch = cp.begin_epoch();
        cp.snapshot(&file); // records existed: false
        std::fs::write(&file, "created").unwrap();
        assert!(file.exists());

        let restored = cp.undo_latest().unwrap().expect("epoch exists");
        assert_eq!(restored, 1);
        assert!(!file.exists(), "undo must remove created files");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn snapshots_are_scoped_per_epoch_and_latest_wins() {
        let root = tempdir("scoped");
        let cp = Checkpointing::new(root.join("store"));
        let file = root.join("f.txt");

        std::fs::write(&file, "v1").unwrap();
        let _e1 = cp.begin_epoch();
        cp.snapshot(&file);
        std::fs::write(&file, "v2").unwrap();

        let _e2 = cp.begin_epoch();
        cp.snapshot(&file);
        std::fs::write(&file, "v3").unwrap();

        // undo only touches the newest epoch → back to v2
        assert_eq!(cp.undo_latest().unwrap(), Some(1));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_without_epochs_is_none() {
        let root = tempdir("empty");
        let cp = Checkpointing::new(root.join("store"));
        assert_eq!(cp.undo_latest().unwrap(), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
