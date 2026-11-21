//! In-process HLSL → DXIL compilation with DXC.
//!
//! `dxcompiler.dll` is not part of Windows, so it is loaded at run time from
//! `%XCL_DXC_DIR%`, the folder of xcl.exe, or the newest Windows SDK
//! (`Windows Kits\10\bin\<ver>\x64`). `dxil.dll` from the same folder is loaded first so
//! the compiler can sign the DXIL (unsigned DXIL is rejected by D3D12).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows::core::{Interface, GUID, HRESULT, PCWSTR};
use windows::Win32::Graphics::Direct3D::Dxc::{DxcBuffer, IDxcBlob, IDxcCompiler3, IDxcResult, CLSID_DxcCompiler, DXC_CP_UTF8};

type CreateInstance = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = std::env::var_os("XCL_DXC_DIR") {
        v.push(PathBuf::from(d));
    }
    if let Some(d) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        v.push(d);
    }
    for root in [r"C:\Program Files (x86)\Windows Kits\10\bin", r"C:\Program Files\Windows Kits\10\bin"] {
        if let Ok(rd) = std::fs::read_dir(root) {
            let mut vers: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            vers.sort();
            for ver in vers.into_iter().rev() {
                v.push(ver.join("x64"));
            }
        }
    }
    v
}

static LIB: OnceLock<Result<(PathBuf, usize), String>> = OnceLock::new();

/// Folder with dxcompiler.dll (+ dxil.dll) that will be used.
pub fn location() -> Result<&'static Path, String> {
    load().map(|(p, _)| p.as_path())
}

fn load() -> Result<&'static (PathBuf, usize), String> {
    LIB.get_or_init(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        for dir in candidates() {
            let dxc = dir.join("dxcompiler.dll");
            if !dxc.is_file() {
                continue;
            }
            unsafe {
                let dxil = dir.join("dxil.dll");
                if dxil.is_file() {
                    LoadLibraryW(crate::util::wide(&dxil.to_string_lossy()).as_ptr());
                }
                let h = LoadLibraryW(crate::util::wide(&dxc.to_string_lossy()).as_ptr());
                if h.is_null() {
                    continue;
                }
                if let Some(f) = GetProcAddress(h, b"DxcCreateInstance\0".as_ptr()) {
                    return Ok((dir, f as usize));
                }
            }
        }
        Err("dxcompiler.dll not found (set XCL_DXC_DIR or install the Windows SDK)".into())
    })
    .as_ref()
    .map_err(|e| e.clone())
}

fn create_fn() -> Result<CreateInstance, String> {
    let &(_, f) = load()?;
    Ok(unsafe { std::mem::transmute::<usize, CreateInstance>(f) })
}

thread_local! {
    static COMPILER: RefCell<Option<IDxcCompiler3>> = const { RefCell::new(None) };
}

fn compiler() -> Result<IDxcCompiler3, String> {
    COMPILER.with(|c| {
        if let Some(x) = c.borrow().as_ref() {
            return Ok(x.clone());
        }
        let create = create_fn()?;
        let mut p = std::ptr::null_mut();
        let hr = unsafe { create(&CLSID_DxcCompiler, &IDxcCompiler3::IID, &mut p) };
        if hr.is_err() || p.is_null() {
            return Err(format!("DxcCreateInstance: {hr:?}"));
        }
        let x = unsafe { IDxcCompiler3::from_raw(p) };
        *c.borrow_mut() = Some(x.clone());
        Ok(x)
    })
}

/// Stack of the thread DXC runs on: its LLVM passes recurse along long dependency chains of
/// big translated programs (2 MB overflowed on a 7 000-instruction retail PS); the caller may
/// be a game thread with a small stack. Reserved, committed only as used.
const DXC_STACK: usize = 64 << 20;

struct Job {
    src: String,
    entry: String,
    profile: String,
    extra: Vec<String>,
    reply: std::sync::mpsc::Sender<Result<Vec<u8>, String>>,
}

/// DXC workers (one per core, each with its own compiler and a `DXC_STACK` stack), started
/// on first use; callers wait for their job.
fn pool() -> &'static std::sync::Mutex<std::sync::mpsc::Sender<Job>> {
    static POOL: OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<Job>>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
        for k in 0..std::thread::available_parallelism().map_or(4, |n| n.get()) {
            let rx = rx.clone();
            let _ = std::thread::Builder::new().name(format!("xcl-dxc-{k}")).stack_size(DXC_STACK).spawn(move || loop {
                let job = match rx.lock().unwrap().recv() {
                    Ok(j) => j,
                    Err(_) => return,
                };
                let extra: Vec<&str> = job.extra.iter().map(|s| s.as_str()).collect();
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| compile_here(&job.src, &job.entry, &job.profile, &extra)))
                    .unwrap_or_else(|_| Err("DXC: panic in the compile thread".into()));
                let _ = job.reply.send(r);
            });
        }
        std::sync::Mutex::new(tx)
    })
}

/// Compile HLSL to signed DXIL. `profile` e.g. "cs_6_6"; `extra` are dxc arguments.
pub fn compile(src: &str, entry: &str, profile: &str, extra: &[&str]) -> Result<Vec<u8>, String> {
    let (reply, rx) = std::sync::mpsc::channel();
    let job = Job { src: src.into(), entry: entry.into(), profile: profile.into(), extra: extra.iter().map(|s| s.to_string()).collect(), reply };
    pool().lock().unwrap().send(job).map_err(|_| "DXC: no compile threads".to_string())?;
    rx.recv().map_err(|_| "DXC: the compile thread exited".to_string())?
}

fn compile_here(src: &str, entry: &str, profile: &str, extra: &[&str]) -> Result<Vec<u8>, String> {
    let c = compiler()?;
    let mut args: Vec<Vec<u16>> = ["-E", entry, "-T", profile, "-HV", "2021"].iter().map(|a| crate::util::wide(a)).collect();
    // DXC's optimiser was superlinear on translated programs full of short-circuit `?:` /
    // `&&` / `if` per instruction (a 1589-instruction console CS: ~6 min and > 30 GB at -O3).
    // With branch-free emission (select / and / or) it is fast: that shader 0.3 s, a
    // 15 720-instruction one 6 s (585 KB DXIL vs 1.6 MB at -O0, which the driver could not
    // compile). -O0 only as a safety net for enormous sources.
    if src.len() > 8 * 1024 * 1024 && !extra.iter().any(|a| a.starts_with("-O")) {
        args.push(crate::util::wide("-O0"));
    }
    args.extend(extra.iter().map(|a| crate::util::wide(a)));
    let ptrs: Vec<PCWSTR> = args.iter().map(|a| PCWSTR(a.as_ptr())).collect();
    let buf = DxcBuffer { Ptr: src.as_ptr() as *const c_void, Size: src.len(), Encoding: DXC_CP_UTF8.0 };
    unsafe {
        let r: IDxcResult = c.Compile(&buf, Some(&ptrs), None).map_err(|e| format!("Compile: {e}"))?;
        let status = r.GetStatus().map_err(|e| format!("{e}"))?;
        if status.is_err() {
            let msg = r
                .GetErrorBuffer()
                .ok()
                .map(|b| {
                    let s = std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
                    String::from_utf8_lossy(s).trim_end_matches('\0').to_string()
                })
                .unwrap_or_default();
            return Err(format!("dxc: {msg}"));
        }
        let blob: IDxcBlob = r.GetResult().map_err(|e| format!("{e}"))?;
        Ok(std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()).to_vec())
    }
}

fn toolchain_identity() -> &'static str {
    static IDENTITY: OnceLock<String> = OnceLock::new();
    IDENTITY.get_or_init(|| {
        let Ok(dir) = location() else {
            return "unavailable".into();
        };
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut buffer = [0u8; 64 * 1024];
        for name in ["dxcompiler.dll", "dxil.dll"] {
            for byte in name.bytes().chain([0]) {
                h ^= byte as u64;
                h = h.wrapping_mul(0x100_0000_01b3);
            }
            let Ok(mut file) = std::fs::File::open(dir.join(name)) else {
                h ^= 0xff;
                h = h.wrapping_mul(0x100_0000_01b3);
                continue;
            };
            loop {
                let Ok(read) = file.read(&mut buffer) else {
                    h ^= 0xfe;
                    h = h.wrapping_mul(0x100_0000_01b3);
                    break;
                };
                if read == 0 {
                    break;
                }
                for &byte in &buffer[..read] {
                    h ^= byte as u64;
                    h = h.wrapping_mul(0x100_0000_01b3);
                }
            }
        }
        format!("{h:016x}")
    })
}

/// FNV-1a 64 of the inputs that determine the DXIL.
fn cache_key_for_toolchain(src: &str, entry: &str, profile: &str, toolchain: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for part in [env!("CARGO_PKG_VERSION"), toolchain, profile, entry, src] {
        for b in part.bytes().chain([0u8]) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

fn cache_key(src: &str, entry: &str, profile: &str) -> u64 {
    cache_key_for_toolchain(src, entry, profile, toolchain_identity())
}

/// Folder of the DXIL cache: `XCL_DXIL_CACHE`, else `%LOCALAPPDATA%\xcl\dxil`.
pub fn cache_dir() -> PathBuf {
    if let Ok(d) = std::env::var("XCL_DXIL_CACHE") {
        return PathBuf::from(d);
    }
    let base = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    base.join("xcl").join("dxil")
}

/// `compile` through a disk cache in `dir` (big translated programs take seconds in DXC;
/// the same console shader comes back every run). Returns (DXIL, from cache).
pub fn compile_cached_in(dir: &Path, src: &str, entry: &str, profile: &str) -> Result<(Vec<u8>, bool), String> {
    compile_cached_in_with(dir, src, entry, profile, || compile(src, entry, profile, &[]))
}

type CacheFlight = (PathBuf, u64);

fn cache_flight(key: CacheFlight) -> Arc<Mutex<()>> {
    static FLIGHTS: OnceLock<Mutex<HashMap<CacheFlight, Weak<Mutex<()>>>>> = OnceLock::new();
    let mut flights = FLIGHTS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    flights.retain(|_, v| v.strong_count() != 0);
    if let Some(flight) = flights.get(&key).and_then(Weak::upgrade) {
        return flight;
    }
    let flight = Arc::new(Mutex::new(()));
    flights.insert(key, Arc::downgrade(&flight));
    flight
}

struct ProcessCacheLock {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

impl Drop for ProcessCacheLock {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.handle);
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

fn cache_mutex_name(file: &Path) -> String {
    let absolute = file
        .parent()
        .and_then(|parent| file.file_name().map(|name| normalized_cache_dir(parent).join(name)))
        .unwrap_or_else(|| std::path::absolute(file).unwrap_or_else(|_| file.to_path_buf()));
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in absolute.to_string_lossy().to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!(r"Local\OlympusDxilCache-{h:016x}")
}

fn normalized_cache_dir(dir: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(dir) {
        return canonical;
    }
    let absolute = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    for ancestor in absolute.ancestors().skip(1) {
        if let Ok(canonical) = std::fs::canonicalize(ancestor) {
            if let Ok(remainder) = absolute.strip_prefix(ancestor) {
                return canonical.join(remainder);
            }
        }
    }
    absolute
}

fn process_cache_lock(file: &Path) -> Result<ProcessCacheLock, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject, INFINITE};
    let name = crate::util::wide(&cache_mutex_name(file));
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err("DXIL cache: cannot create the cross-process mutex".into());
    }
    let wait = unsafe { WaitForSingleObject(handle, INFINITE) };
    if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
        unsafe { CloseHandle(handle) };
        return Err(format!("DXIL cache: waiting for the cross-process mutex failed ({wait:#x})"));
    }
    Ok(ProcessCacheLock { handle })
}

fn cached_file(file: &Path) -> Option<Vec<u8>> {
    if let Ok(b) = std::fs::read(&file) {
        if valid_dxil_container(&b) {
            return Some(b);
        }
    }
    None
}

/// Cheap validation before handing a cached blob to D3D12. DXIL uses the DXBC
/// container header: magic, hash, version, total size, chunk count and offsets.
fn valid_dxil_container(blob: &[u8]) -> bool {
    let rd = |at: usize| {
        blob.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    if blob.len() < 36 || blob.get(..4) != Some(b"DXBC") || rd(20) != Some(1) || rd(24) != Some(blob.len()) {
        return false;
    }
    let Some(count) = rd(28) else { return false };
    if count == 0 || count > 64 {
        return false;
    }
    let Some(table_end) = 32usize.checked_add(count * 4) else { return false };
    if table_end > blob.len() {
        return false;
    }
    let mut ranges = Vec::with_capacity(count);
    let mut has_dxil = false;
    for i in 0..count {
        let Some(off) = rd(32 + i * 4) else { return false };
        if off < table_end || off & 3 != 0 {
            return false;
        }
        let Some(size) = rd(off + 4) else { return false };
        let Some(end) = off.checked_add(8).and_then(|v| v.checked_add(size)) else { return false };
        if end > blob.len() || ranges.iter().any(|&(a, b)| off < b && a < end) {
            return false;
        }
        if blob.get(off..off + 4) == Some(b"DXIL") {
            // DxilProgramHeader is six dwords before the bitcode payload.
            if size < 24 || blob.get(off + 16..off + 20) != Some(b"DXIL") {
                return false;
            }
            has_dxil = true;
        }
        ranges.push((off, end));
    }
    has_dxil
}

fn compile_cached_in_with<F>(dir: &Path, src: &str, entry: &str, profile: &str, compile_one: F) -> Result<(Vec<u8>, bool), String>
where
    F: FnOnce() -> Result<Vec<u8>, String>,
{
    let key = cache_key(src, entry, profile);
    let dir = normalized_cache_dir(dir);
    let file = dir.join(format!("{key:016x}.dxil"));
    if let Some(b) = cached_file(&file) {
        return Ok((b, true));
    }
    let flight = cache_flight((dir.clone(), key));
    let _guard = flight.lock().unwrap_or_else(|e| e.into_inner());
    let _process_guard = process_cache_lock(&file)?;
    // Another caller may have populated the cache while this one waited.
    if let Some(b) = cached_file(&file) {
        return Ok((b, true));
    }
    if CACHED_ONLY.with(|c| c.get()) {
        MISSED.with(|m| m.set(true));
        return Err("DXIL not compiled yet".into());
    }
    // A corrupt target must be removed before the atomic rename on Windows;
    // rename does not replace an existing file there.
    if file.is_file() {
        let _ = std::fs::remove_file(&file);
    }
    let b = compile_one()?;
    if !valid_dxil_container(&b) {
        return Err("DXIL cache: compiler returned an invalid DXIL container".into());
    }
    if std::fs::create_dir_all(&dir).is_ok() {
        // write to a temporary name first: another thread / process may be reading
        let tmp = file.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, &b).is_ok() && std::fs::rename(&tmp, &file).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    Ok((b, false))
}

thread_local! {
    static CACHED_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static MISSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` without compiling: a program missing from the disk cache fails at once instead
/// (non-blocking pipeline creation). Returns `f`'s result and whether something was missing.
pub fn cached_only<R>(f: impl FnOnce() -> R) -> (R, bool) {
    CACHED_ONLY.with(|c| c.set(true));
    MISSED.with(|m| m.set(false));
    let r = f();
    CACHED_ONLY.with(|c| c.set(false));
    (r, MISSED.with(|m| m.replace(false)))
}

/// Inside `cached_only`.
pub fn in_cached_only() -> bool {
    CACHED_ONLY.with(|c| c.get())
}

pub fn compile_cached(src: &str, entry: &str, profile: &str) -> Result<(Vec<u8>, bool), String> {
    compile_cached_in(&cache_dir(), src, entry, profile)
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn synthetic_dxil(marker: u8) -> Vec<u8> {
        let mut b = vec![0u8; 68];
        b[..4].copy_from_slice(b"DXBC");
        b[20..24].copy_from_slice(&1u32.to_le_bytes());
        b[24..28].copy_from_slice(&68u32.to_le_bytes());
        b[28..32].copy_from_slice(&1u32.to_le_bytes());
        b[32..36].copy_from_slice(&36u32.to_le_bytes());
        b[36..40].copy_from_slice(b"DXIL");
        b[40..44].copy_from_slice(&24u32.to_le_bytes());
        b[52..56].copy_from_slice(b"DXIL"); // inner DxilBitcodeHeader magic
        b[67] = marker;
        b
    }

    #[test]
    fn dxil_cache_round_trip() {
        if location().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("xcl-dxil-test-{}", std::process::id()));
        let src = "[numthreads(1, 1, 1)] void main() {}";
        let (a, hit) = compile_cached_in(&dir, src, "main", "cs_6_6").unwrap();
        assert!(!hit);
        let (b, hit) = compile_cached_in(&dir, src, "main", "cs_6_6").unwrap();
        assert!(hit && a == b);
        assert_ne!(cache_key(src, "main", "cs_6_6"), cache_key(src, "main", "cs_6_7"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_key_tracks_the_dxc_toolchain() {
        let a = cache_key_for_toolchain("source", "main", "cs_6_6", "compiler-a/signer-a");
        assert_eq!(a, cache_key_for_toolchain("source", "main", "cs_6_6", "compiler-a/signer-a"));
        assert_ne!(a, cache_key_for_toolchain("source", "main", "cs_6_6", "compiler-b/signer-a"));
        assert_ne!(a, cache_key_for_toolchain("source", "main", "cs_6_6", "compiler-a/signer-b"));
    }

    #[test]
    fn validates_dxil_cache_structure_and_rebuilds_corruption() {
        let valid = synthetic_dxil(7);
        assert!(valid_dxil_container(&valid));
        assert!(!valid_dxil_container(&valid[..valid.len() - 1]));
        let mut bad = valid.clone();
        bad[0] = b'X';
        assert!(!valid_dxil_container(&bad));
        let mut bad = valid.clone();
        bad[24..28].copy_from_slice(&999u32.to_le_bytes());
        assert!(!valid_dxil_container(&bad));
        let mut bad = valid.clone();
        bad[32..36].copy_from_slice(&35u32.to_le_bytes());
        assert!(!valid_dxil_container(&bad));
        let mut bad = valid.clone();
        bad[36..40].copy_from_slice(b"STAT");
        assert!(!valid_dxil_container(&bad));
        let mut bad = valid.clone();
        bad[52..56].fill(0);
        assert!(!valid_dxil_container(&bad));

        let mut overlap = vec![0u8; 80];
        overlap[..4].copy_from_slice(b"DXBC");
        overlap[20..24].copy_from_slice(&1u32.to_le_bytes());
        overlap[24..28].copy_from_slice(&80u32.to_le_bytes());
        overlap[28..32].copy_from_slice(&2u32.to_le_bytes());
        overlap[32..36].copy_from_slice(&40u32.to_le_bytes());
        overlap[36..40].copy_from_slice(&40u32.to_le_bytes());
        overlap[40..44].copy_from_slice(b"DXIL");
        overlap[44..48].copy_from_slice(&24u32.to_le_bytes());
        overlap[56..60].copy_from_slice(b"DXIL");
        assert!(!valid_dxil_container(&overlap));

        let dir = std::env::temp_dir().join(format!("olympus-dxil-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = cache_key("corrupt", "main", "cs_6_6");
        let file = dir.join(format!("{key:016x}.dxil"));
        std::fs::write(&file, b"truncated cache entry").unwrap();
        let calls = AtomicUsize::new(0);
        let (rebuilt, hit) = compile_cached_in_with(&dir, "corrupt", "main", "cs_6_6", || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(synthetic_dxil(8))
        })
        .unwrap();
        assert!(!hit);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(rebuilt, synthetic_dxil(8));
        let (_, hit) = compile_cached_in_with(&dir, "corrupt", "main", "cs_6_6", || panic!("valid rebuilt entry was not reused")).unwrap();
        assert!(hit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_invalid_fresh_dxil_without_poisoning_the_cache() {
        let dir = std::env::temp_dir().join(format!("olympus-dxil-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let key = cache_key("fresh validation", "main", "cs_6_6");
        let file = dir.join(format!("{key:016x}.dxil"));

        let error = compile_cached_in_with(&dir, "fresh validation", "main", "cs_6_6", || Ok(b"not DXIL".to_vec())).unwrap_err();
        assert!(error.contains("invalid DXIL container"));
        assert!(!file.exists());

        let valid = synthetic_dxil(11);
        let (rebuilt, hit) = compile_cached_in_with(&dir, "fresh validation", "main", "cs_6_6", || Ok(valid.clone())).unwrap();
        assert!(!hit);
        assert_eq!(rebuilt, valid);
        let (_, hit) = compile_cached_in_with(&dir, "fresh validation", "main", "cs_6_6", || panic!("valid retry was not cached")).unwrap();
        assert!(hit);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_cache_misses_compile_once() {
        let dir = std::env::temp_dir().join(format!("olympus-dxil-flight-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let calls = AtomicUsize::new(0);
        let hits = AtomicUsize::new(0);
        std::thread::scope(|s| {
            let mut threads = Vec::new();
            for _ in 0..8 {
                threads.push(s.spawn(|| {
                    let (dxil, hit) = compile_cached_in_with(&dir, "same source", "main", "cs_6_6", || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(25));
                        Ok(synthetic_dxil(4))
                    })
                    .unwrap();
                    assert_eq!(dxil, synthetic_dxil(4));
                    hits.fetch_add(hit as usize, Ordering::SeqCst);
                }));
            }
            for thread in threads {
                thread.join().unwrap();
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(hits.load(Ordering::SeqCst), 7);

        // A failed leader releases the flight; the next caller can retry.
        let failed = compile_cached_in_with(&dir, "retry", "main", "cs_6_6", || Err("synthetic failure".into()));
        assert!(failed.is_err());
        let (dxil, hit) = compile_cached_in_with(&dir, "retry", "main", "cs_6_6", || Ok(synthetic_dxil(9))).unwrap();
        assert!(!hit);
        assert_eq!(dxil, synthetic_dxil(9));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn different_cache_keys_do_not_block_each_other() {
        let dir = std::env::temp_dir().join(format!("olympus-dxil-parallel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|s| {
            let a = s.spawn(|| compile_cached_in_with(&dir, "source a", "main", "cs_6_6", || {
                barrier.wait();
                Ok(synthetic_dxil(1))
            }));
            let b = s.spawn(|| compile_cached_in_with(&dir, "source b", "main", "cs_6_6", || {
                barrier.wait();
                Ok(synthetic_dxil(2))
            }));
            assert_eq!(a.join().unwrap().unwrap().0, synthetic_dxil(1));
            assert_eq!(b.join().unwrap().unwrap().0, synthetic_dxil(2));
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_aliases_share_the_same_cache_flight() {
        let root = std::env::temp_dir().join(format!("olympus-dxil-alias-{}", std::process::id()));
        let dir = root.join("cache");
        let child = dir.join("child");
        let alias = child.join("..");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&child).unwrap();
        assert_eq!(normalized_cache_dir(&dir), normalized_cache_dir(&alias));
        assert_eq!(normalized_cache_dir(&root.join("future")), normalized_cache_dir(&child.join("..").join("..").join("future")));
        let key = cache_key("aliased source", "main", "cs_6_6");
        assert_eq!(
            cache_mutex_name(&dir.join(format!("{key:016x}.dxil"))),
            cache_mutex_name(&alias.join(format!("{key:016x}.dxil")))
        );

        let start = std::sync::Barrier::new(2);
        let calls = AtomicUsize::new(0);
        let hits = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                start.wait();
                compile_cached_in_with(&dir, "aliased source", "main", "cs_6_6", || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    Ok(synthetic_dxil(12))
                })
                .unwrap()
            });
            let second = scope.spawn(|| {
                start.wait();
                compile_cached_in_with(&alias, "aliased source", "main", "cs_6_6", || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    Ok(synthetic_dxil(12))
                })
                .unwrap()
            });
            for result in [first.join().unwrap(), second.join().unwrap()] {
                assert_eq!(result.0, synthetic_dxil(12));
                hits.fetch_add(result.1 as usize, Ordering::SeqCst);
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn process_cache_mutex_serializes_the_same_file() {
        let file = std::env::temp_dir().join(format!("olympus-dxil-mutex-{}-{}.dxil", std::process::id(), cache_key("mutex", "main", "cs_6_6")));
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (second_tx, second_rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            let first_file = file.clone();
            s.spawn(move || {
                let _guard = process_cache_lock(&first_file).unwrap();
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            held_rx.recv().unwrap();
            s.spawn(move || {
                let _guard = process_cache_lock(&file).unwrap();
                second_tx.send(()).unwrap();
            });
            assert!(second_rx.recv_timeout(Duration::from_millis(30)).is_err());
            release_tx.send(()).unwrap();
            second_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        });
    }
}
