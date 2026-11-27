//! A recipe can be left on disk only after a successful replay identified every
//! native blob it used. A requested or previously bound recipe is restored live.
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Interrupted writes or a damaged index fall back to ordinary preparation.
pub(crate) fn read_receipt<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    let (checksum, payload) = bytes.split_at_checked(8)?;
    if checksum != crate::gpu::shader::fnv64(payload).to_le_bytes() { return None; }
    serde_json::from_slice(payload).ok()
}

pub(crate) fn write_receipt<T: Serialize>(path: &Path, receipt: &T) -> bool {
    let mut bytes = vec![0; 8];
    if serde_json::to_writer(&mut bytes, receipt).is_err() { return false; }
    let checksum = crate::gpu::shader::fnv64(&bytes[8..]).to_le_bytes();
    bytes[..8].copy_from_slice(&checksum);
    crate::hle::d3d12x::real::driver_cache::write(path, &bytes)
}

pub(crate) type Stamp = [u64; 3];
pub(crate) type Inventory = HashMap<String, Stamp>;

fn stamp(m: &std::fs::Metadata) -> Option<Stamp> {
    (m.is_file() && m.len() != 0).then(|| [m.len(), m.creation_time(), m.last_write_time()])
}

pub(crate) fn driver_files(dir: &Path) -> Option<Inventory> {
    let mut files = Inventory::new();
    for e in std::fs::read_dir(dir).ok()? {
        let Ok(e) = e else { continue };
        let Ok(name) = e.file_name().into_string() else { continue };
        if !name.ends_with(".pso") { continue; }
        let Some(stamp) = e.metadata().ok().and_then(|m| stamp(&m)) else { continue };
        files.insert(name, stamp);
    }
    Some(files)
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct File { name: String, stamp: Stamp }
type Recipes = HashMap<u64, Vec<File>>;

struct Captured { files: Vec<File>, complete: bool }
thread_local! { static CAPTURE: RefCell<Option<Captured>> = const { RefCell::new(None) }; }

pub(crate) fn capture(f: impl FnOnce() -> bool) -> (bool, Vec<File>) {
    CAPTURE.with(|slot| { *slot.borrow_mut() = Some(Captured { files: Vec::new(), complete: true }); });
    let success = f();
    let files = CAPTURE.with(|slot| {
        let captured = slot.take().unwrap();
        if captured.complete { captured.files } else { Vec::new() }
    });
    (success, files)
}

/// Call for each successful pass. A memory-cache hit must also match the bytes in the
/// corresponding disk file. Any pass without a verified blob invalidates the recipe.
pub(crate) fn accepted(passes: &[(Option<&Path>, Option<&Arc<Vec<u8>>>, bool)]) {
    CAPTURE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(captured) = slot.as_mut() else { return };
        let checked = passes.iter().map(|&(path, bytes, disk)| {
            let path = path?;
            let blob = bytes?;
            let stamp = stamp(&path.metadata().ok()?)?;
            if stamp[0] != blob.len() as u64 { return None; }
            if !disk && std::fs::read(path).ok()?.as_slice() != blob.as_slice() { return None; }
            Some(File { name: path.file_name()?.to_str()?.to_owned(), stamp })
        }).collect::<Option<Vec<_>>>();
        if let Some(files) = checked { captured.files.extend(files); }
        else { captured.complete = false; }
    });
}

pub(super) struct Index {
    path: PathBuf,
    state: Mutex<IndexState>,
}

struct IndexState { entries: Recipes, unflushed: usize }

const INDEX_BATCH: usize = 1024;

impl Index {
    pub(super) fn open(dir: &Path, known: &Inventory, key: u64, windows: u8) -> Option<Self> {
        let path = dir.join(format!("warm-recipes-v2-{key:016x}-{windows:02x}.bin"));
        let mut entries: Recipes = read_receipt(&path).unwrap_or_default();
        entries.retain(|_, files| !files.is_empty() && files.iter().all(|f| known.get(&f.name) == Some(&f.stamp)));
        Some(Self { path, state: Mutex::new(IndexState { entries, unflushed: 0 }) })
    }
    pub(super) fn can_defer(&self, hash: u64) -> bool { self.state.lock().unwrap().entries.contains_key(&hash) }
    pub(super) fn add(&self, hash: u64, files: Vec<File>) {
        if files.is_empty() { return; }
        let mut state = self.state.lock().unwrap();
        state.entries.insert(hash, files);
        state.unflushed += 1;
        if state.unflushed >= INDEX_BATCH && state.unflushed % INDEX_BATCH == 0 {
            Self::flush(&self.path, &mut state);
        }
    }
    fn flush(path: &Path, state: &mut IndexState) {
        if write_receipt(path, &state.entries) {
            state.unflushed = 0;
        } else {
            crate::xlog_warn!("PSO preparation: could not save the verified recipe index");
        }
    }
    pub(super) fn save(self) {
        let mut state = self.state.into_inner().unwrap();
        if state.unflushed != 0 { Self::flush(&self.path, &mut state); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_index_survives_interruption_after_a_batch() {
        let dir = std::env::temp_dir().join(format!("xcl-index-batch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let blob = dir.join("shared.pso");
        std::fs::write(&blob, [1, 2, 3]).unwrap();
        let file = File { name: "shared.pso".into(), stamp: stamp(&blob.metadata().unwrap()).unwrap() };
        let book = Index::open(&dir, &driver_files(&dir).unwrap(), 7, 4).unwrap();
        for hash in 0..INDEX_BATCH as u64 { book.add(hash, vec![file.clone()]); }
        let reopened = Index::open(&dir, &driver_files(&dir).unwrap(), 7, 4).unwrap();
        assert!(reopened.can_defer(0) && reopened.can_defer(INDEX_BATCH as u64 - 1));
        book.add(INDEX_BATCH as u64, vec![file]);
        assert!(!Index::open(&dir, &driver_files(&dir).unwrap(), 7, 4).unwrap().can_defer(INDEX_BATCH as u64));
        book.save();
        assert!(Index::open(&dir, &driver_files(&dir).unwrap(), 7, 4).unwrap().can_defer(INDEX_BATCH as u64));
        std::fs::remove_file(dir.join("warm-recipes-v2-0000000000000007-04.bin")).unwrap();
        std::fs::remove_file(blob).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn every_split_piece_needs_a_verified_blob() {
        let dir = std::env::temp_dir().join(format!("xcl-split-receipt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("first.pso");
        let second = dir.join("second.pso");
        let bytes = Arc::new(vec![1, 2, 3]);
        std::fs::write(&first, &*bytes).unwrap();
        std::fs::write(&second, &*bytes).unwrap();
        let (_, files) = capture(|| {
            accepted(&[(Some(&first), Some(&bytes), true)]);
            accepted(&[(Some(&second), Some(&bytes), true)]);
            true
        });
        assert_eq!(files.len(), 2);
        std::fs::remove_file(&second).unwrap();
        let (_, files) = capture(|| {
            accepted(&[(Some(&first), Some(&bytes), true)]);
            accepted(&[(Some(&second), Some(&bytes), true)]);
            true
        });
        assert!(files.is_empty());
        std::fs::remove_file(&first).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn both_passes_must_survive_for_a_warm_recipe_to_leave_the_loading_screen() {
        let dir = std::env::temp_dir().join(format!("xcl-verified-replay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let g = dir.join("g.pso");
        let c = dir.join("c.pso");
        let bytes = Arc::new(vec![1, 2, 3]);
        std::fs::write(&g, &*bytes).unwrap();
        std::fs::write(&c, &*bytes).unwrap();
        let book = Index::open(&dir, &driver_files(&dir).unwrap(), 1, 4).unwrap();
        let (_, files) = capture(|| {
            accepted(&[(Some(&g), Some(&bytes), true), (Some(&c), Some(&bytes), false)]);
            true
        });
        assert_eq!(files.len(), 2);
        book.add(23, files);
        book.save();
        assert!(Index::open(&dir, &driver_files(&dir).unwrap(), 1, 4).unwrap().can_defer(23));
        assert!(!Index::open(&dir, &driver_files(&dir).unwrap(), 2, 4).unwrap().can_defer(23));
        assert!(!Index::open(&dir, &driver_files(&dir).unwrap(), 1, 3).unwrap().can_defer(23));
        std::fs::write(&c, [9; 4]).unwrap();
        assert!(!Index::open(&dir, &driver_files(&dir).unwrap(), 1, 4).unwrap().can_defer(23));
        let (_, files) = capture(|| {
            accepted(&[(Some(&g), Some(&bytes), true), (Some(&c), Some(&bytes), false)]);
            true
        });
        assert!(files.is_empty());
        std::fs::remove_file(&g).unwrap();
        assert!(!Index::open(&dir, &driver_files(&dir).unwrap(), 1, 4).unwrap().can_defer(23));
        std::fs::remove_file(&c).unwrap();
        std::fs::remove_file(dir.join("warm-recipes-v2-0000000000000001-04.bin")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
