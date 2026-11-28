//! Successful temporary-device validations. Live PSO creation on the render device
//! still supplies the driver blob and retries normally if the driver refuses it.
use std::collections::{hash_map::DefaultHasher, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{atomic::{AtomicBool, Ordering}, Mutex};
use windows::Win32::Graphics::Direct3D12::ID3D12Device;

pub(super) fn digest(value: &impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

pub(super) fn stamp(path: &Path, layout: u64, blob: &[u8]) -> [u64; 3] {
    [digest(&path.file_name()), layout, digest(&blob)]
}

pub(super) struct Verified {
    path: PathBuf,
    keys: Mutex<HashSet<[u64; 3]>>,
    dirty: AtomicBool,
}

impl Verified {
    pub(super) fn for_device(dev: &ID3D12Device) -> Option<Self> {
        let key = super::warm_replay::native_key(dev)?;
        let slice = std::env::var("XCL_PRECOMPILE_SLICE").unwrap_or_default();
        let key = digest(&(key, slice));
        Some(Self::open(crate::gpu::recomp::dxc::cache_dir().join(format!("verified-compute-v1-{key:016x}.bin"))))
    }

    fn open(path: PathBuf) -> Self {
        let mut keys = HashSet::new();
        if let Ok(bytes) = std::fs::read(&path) {
            if let Some((checksum, body)) = bytes.split_at_checked(8) {
                if body.len() % 24 == 0 && checksum == digest(&body).to_le_bytes() {
                    for row in body.chunks_exact(24) {
                        keys.insert(std::array::from_fn(|i| u64::from_le_bytes(row[i*8..i*8+8].try_into().unwrap())));
                    }
                }
            }
        }
        Self { path, keys: Mutex::new(keys), dirty: AtomicBool::new(false) }
    }

    pub(super) fn contains(&self, stamp: &[u64; 3]) -> bool {
        self.keys.lock().unwrap().contains(stamp)
    }

    pub(super) fn record(&self, stamp: [u64; 3]) {
        if self.keys.lock().unwrap().insert(stamp) { self.dirty.store(true, Ordering::Relaxed); }
    }

    pub(super) fn save(&self) {
        if !self.dirty.load(Ordering::Relaxed) { return; }
        let keys = self.keys.lock().unwrap();
        let mut data = Vec::with_capacity(8 + 24 * keys.len());
        data.extend_from_slice(&[0; 8]);
        for row in keys.iter() {
            for v in row { data.extend_from_slice(&v.to_le_bytes()); }
        }
        let sum = digest(&&data[8..]).to_le_bytes();
        data[..8].copy_from_slice(&sum);
        if !crate::hle::d3d12x::real::driver_cache::write(&self.path, &data) {
            crate::xlog_warn!("Shader preparation: could not save verified compute blob list");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn warm_receipt_matches_only_the_validated_blob_and_layout() {
        let path = std::env::temp_dir().join(format!("xcl-warm-receipt-{}.bin", std::process::id()));
        let file = Path::new("123.chain.pso");
        let key = stamp(file, 1, &[3, 4]);
        let book = Verified::open(path.clone());
        book.record(key);
        book.save();
        let book = Verified::open(path.clone());
        assert!(book.contains(&key));
        assert!(!book.contains(&stamp(file, 2, &[3, 4])));
        assert!(!book.contains(&stamp(file, 1, &[3, 5])));
        assert!(!book.contains(&stamp(Path::new("123.pso"), 1, &[3, 4])));
        let mut contents = std::fs::read(&path).unwrap();
        contents[8] ^= 1;
        std::fs::write(&path, contents).unwrap();
        assert!(!Verified::open(path.clone()).contains(&key));
        std::fs::remove_file(path).unwrap();
    }
}
