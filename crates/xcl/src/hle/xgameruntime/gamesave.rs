//! XGameSave – containers of named blobs (public GDK `XGameSave.h`), stored as files:
//! `<data>/gamesave/<configuration id>/<container>/b/<blob>` + `display` (display name).
//! Offline: no cloud sync, every operation completes at once (async variants through XAsync).

use std::ffi::{c_void, CString};
use std::path::{Path, PathBuf};

use super::xasync::*;
use crate::util::{from_cstr_ptr, E_FAIL, E_INVALIDARG, HRESULT, S_OK};

pub const E_GS_INVALID_CONTAINER_NAME: HRESULT = 0x8083_0001u32 as i32;
pub const E_GS_PROVIDED_BUFFER_TOO_SMALL: HRESULT = 0x8083_0007u32 as i32;
pub const E_GS_BLOB_NOT_FOUND: HRESULT = 0x8083_0008u32 as i32;
pub const E_GS_CONTAINER_NOT_FOUND: HRESULT = 0x8083_0010u32 as i32;
/// quota reported to the title (the GDK default)
pub const QUOTA: i64 = 256 << 20;

const MAGIC_PROVIDER: u32 = 0x5053_4758; // "XGSP"
const MAGIC_CONTAINER: u32 = 0x4353_4758;
const MAGIC_UPDATE: u32 = 0x5553_4758;

pub struct Provider {
    magic: u32,
    root: PathBuf,
}

pub struct Container {
    magic: u32,
    name: String,
    dir: PathBuf,
}

pub struct Update {
    magic: u32,
    dir: PathBuf,
    display: Option<String>,
    /// blob name → new contents (None: delete)
    ops: Vec<(String, Option<Vec<u8>>)>,
}

#[repr(C)]
pub struct ContainerInfo {
    pub name: *const u8,
    pub display_name: *const u8,
    pub blob_count: u32,
    pub total_size: u64,
    pub last_modified_time: i64,
    pub needs_sync: bool,
}

#[repr(C)]
pub struct BlobInfo {
    pub name: *const u8,
    pub size: u32,
}

#[repr(C)]
pub struct Blob {
    pub info: BlobInfo,
    pub data: *mut u8,
}

type ContainerCb = unsafe extern "system" fn(*const ContainerInfo, *mut c_void) -> bool;
type BlobCb = unsafe extern "system" fn(*const BlobInfo, *mut c_void) -> bool;

/// `stem` – a file name's bytes before the first '.' – is a Windows reserved
/// device name (CON, PRN, AUX, NUL, CLOCK$, COM1-9, LPT1-9), in any letter
/// case. Windows opens the device itself for those names, extension or not
/// ("con.txt" still addresses CON), so such a stem must never reach the disk
/// unchanged.
fn device_stem(stem: &[u8]) -> bool {
    let up = stem.to_ascii_uppercase();
    matches!(up.as_slice(), b"CON" | b"PRN" | b"AUX" | b"NUL" | b"CLOCK$")
        || (up.len() == 4 && matches!(&up[..3], b"COM" | b"LPT") && matches!(up[3], b'1'..=b'9'))
}

/// File-name form of a container / blob name: unusual characters as %XX.
///
/// Some names look plain but may never hit the disk as written, so a byte of
/// each is %XX-escaped too:
///
/// - a name ending in '.' or ' ' is stripped by Windows, silently folding it
///   into the shorter name ("x." shares a file with "x"), and bare "." and ".."
///   resolve against the path itself instead of naming a child – a container
///   called ".." would let XGameSaveDeleteContainer remove the provider root's
///   parent. The last byte is escaped ("x." -> "x%2E", ".." -> ".%2E").
/// - a stem that is a reserved device name gets its first byte escaped
///   ("con.txt" -> "%63on.txt").
///
/// Other names keep the exact file name they have always had, and dec()
/// restores every escape like any other %XX.
fn enc(name: &str) -> String {
    let b = name.as_bytes();
    if b.is_empty() {
        return String::new();
    }
    // the stem: bytes before the first '.', less the trailing spaces Windows drops
    let mut stem = b.iter().position(|&c| c == b'.').unwrap_or(b.len());
    while stem > 0 && b[stem - 1] == b' ' {
        stem -= 1;
    }
    let esc_first = device_stem(&b[..stem]);
    let esc_last = matches!(b[b.len() - 1], b'.' | b' ');
    b.iter()
        .enumerate()
        .map(|(i, &c)| {
            if (i == 0 && esc_first) || (i == b.len() - 1 && esc_last) || !(c.is_ascii_alphanumeric() || b"._- ".contains(&c)) {
                format!("%{c:02X}")
            } else {
                (c as char).to_string()
            }
        })
        .collect()
}

fn dec(file: &str) -> String {
    let b = file.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&file[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn blobs_of(dir: &Path) -> Vec<(String, PathBuf, u64)> {
    let mut v: Vec<(String, PathBuf, u64)> = std::fs::read_dir(dir.join("b"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| (dec(&e.file_name().to_string_lossy()), e.path(), e.metadata().map(|m| m.len()).unwrap_or(0)))
        .collect();
    v.sort();
    v
}

unsafe fn provider<'a>(p: *mut Provider) -> Option<&'a Provider> {
    p.as_ref().filter(|p| p.magic == MAGIC_PROVIDER)
}

unsafe fn container<'a>(c: *mut Container) -> Option<&'a Container> {
    c.as_ref().filter(|c| c.magic == MAGIC_CONTAINER)
}

fn open_provider(config: Option<String>) -> *mut Provider {
    let id: String = enc(&config.unwrap_or_else(|| "default".into()));
    let root = crate::state::data_dir().join("gamesave").join(if id.is_empty() { "default".into() } else { id });
    let _ = std::fs::create_dir_all(&root);
    Box::into_raw(Box::new(Provider { magic: MAGIC_PROVIDER, root }))
}

pub unsafe extern "system" fn XGameSaveInitializeProvider(_user: *mut c_void, config: *const u8, _sync_on_demand: bool, out: *mut *mut Provider) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = open_provider(from_cstr_ptr(config));
    S_OK
}

static INIT_ID: u8 = 0;

pub unsafe extern "system" fn XGameSaveInitializeProviderAsync(_user: *mut c_void, config: *const u8, _sync: bool, b: *mut XAsyncBlock) -> HRESULT {
    let config = from_cstr_ptr(config);
    begin_with(b, &INIT_ID as *const u8 as usize, move || Ok((open_provider(config) as usize).to_le_bytes().to_vec()))
}

pub unsafe extern "system" fn XGameSaveInitializeProviderResult(b: *mut XAsyncBlock, out: *mut *mut Provider) -> HRESULT {
    let mut p = 0usize;
    let hr = XAsyncGetResult(b, &INIT_ID as *const u8 as *const c_void, 8, &mut p as *mut usize as *mut c_void, std::ptr::null_mut());
    if hr >= 0 && !out.is_null() {
        *out = p as *mut Provider;
    }
    hr
}

pub unsafe extern "system" fn XGameSaveCloseProvider(p: *mut Provider) {
    if provider(p).is_some() {
        (*p).magic = 0;
        drop(Box::from_raw(p));
    }
}

pub unsafe extern "system" fn XGameSaveGetRemainingQuota(p: *mut Provider, quota: *mut i64) -> HRESULT {
    let Some(pr) = provider(p) else { return E_INVALIDARG };
    if quota.is_null() {
        return E_INVALIDARG;
    }
    let used: u64 = std::fs::read_dir(&pr.root).into_iter().flatten().flatten().flat_map(|e| blobs_of(&e.path())).map(|b| b.2).sum();
    *quota = (QUOTA - used as i64).max(0);
    S_OK
}

static QUOTA_ID: u8 = 0;

pub unsafe extern "system" fn XGameSaveGetRemainingQuotaAsync(p: *mut Provider, b: *mut XAsyncBlock) -> HRESULT {
    let mut q = 0i64;
    let hr = XGameSaveGetRemainingQuota(p, &mut q);
    begin_with(b, &QUOTA_ID as *const u8 as usize, move || if hr < 0 { Err(hr) } else { Ok(q.to_le_bytes().to_vec()) })
}

pub unsafe extern "system" fn XGameSaveGetRemainingQuotaResult(b: *mut XAsyncBlock, quota: *mut i64) -> HRESULT {
    XAsyncGetResult(b, &QUOTA_ID as *const u8 as *const c_void, 8, quota as *mut c_void, std::ptr::null_mut())
}

pub unsafe extern "system" fn XGameSaveDeleteContainer(p: *mut Provider, name: *const u8) -> HRESULT {
    let (Some(pr), Some(n)) = (provider(p), from_cstr_ptr(name)) else { return E_INVALIDARG };
    // an empty name would join to the root itself; reject it like create does
    if n.is_empty() || n.len() > 256 {
        return E_GS_INVALID_CONTAINER_NAME;
    }
    let _ = std::fs::remove_dir_all(pr.root.join(enc(&n)));
    S_OK
}

static DELETE_ID: u8 = 0;

pub unsafe extern "system" fn XGameSaveDeleteContainerAsync(p: *mut Provider, name: *const u8, b: *mut XAsyncBlock) -> HRESULT {
    let hr = XGameSaveDeleteContainer(p, name);
    begin_with(b, &DELETE_ID as *const u8 as usize, move || if hr < 0 { Err(hr) } else { Ok(Vec::new()) })
}

pub unsafe extern "system" fn XGameSaveDeleteContainerResult(b: *mut XAsyncBlock) -> HRESULT {
    XAsyncGetResult(b, &DELETE_ID as *const u8 as *const c_void, 0, std::ptr::null_mut(), std::ptr::null_mut())
}

pub unsafe extern "system" fn XGameSaveCreateContainer(p: *mut Provider, name: *const u8, out: *mut *mut Container) -> HRESULT {
    let (Some(pr), Some(n)) = (provider(p), from_cstr_ptr(name)) else { return E_INVALIDARG };
    if out.is_null() {
        return E_INVALIDARG;
    }
    if n.is_empty() || n.len() > 256 {
        return E_GS_INVALID_CONTAINER_NAME;
    }
    *out = Box::into_raw(Box::new(Container { magic: MAGIC_CONTAINER, dir: pr.root.join(enc(&n)), name: n }));
    S_OK
}

pub unsafe extern "system" fn XGameSaveCloseContainer(c: *mut Container) {
    if container(c).is_some() {
        (*c).magic = 0;
        drop(Box::from_raw(c));
    }
}

/// Call `cb` for every stored container whose name starts with `prefix`.
unsafe fn each_container(pr: &Provider, prefix: &str, exact: bool, ctx: *mut c_void, cb: ContainerCb) -> usize {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&pr.root).into_iter().flatten().flatten().filter(|e| e.path().is_dir()).map(|e| e.path()).collect();
    dirs.sort();
    let mut n = 0;
    for d in dirs {
        let name = dec(&d.file_name().unwrap_or_default().to_string_lossy());
        if (exact && name != prefix) || !name.starts_with(prefix) {
            continue;
        }
        let blobs = blobs_of(&d);
        if blobs.is_empty() && !d.join("display").exists() {
            continue; // created but never written
        }
        let display = std::fs::read_to_string(d.join("display")).unwrap_or_else(|_| name.clone());
        let modified = std::fs::metadata(&d)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|t| t.as_secs() as i64)
            .unwrap_or(0);
        let (cn, cd) = (CString::new(name).unwrap_or_default(), CString::new(display).unwrap_or_default());
        let info = ContainerInfo {
            name: cn.as_ptr() as *const u8,
            display_name: cd.as_ptr() as *const u8,
            blob_count: blobs.len() as u32,
            total_size: blobs.iter().map(|b| b.2).sum(),
            last_modified_time: modified,
            needs_sync: false,
        };
        n += 1;
        if !cb(&info, ctx) {
            break;
        }
    }
    n
}

pub unsafe extern "system" fn XGameSaveGetContainerInfo(p: *mut Provider, name: *const u8, ctx: *mut c_void, cb: Option<ContainerCb>) -> HRESULT {
    let (Some(pr), Some(n), Some(cb)) = (provider(p), from_cstr_ptr(name), cb) else { return E_INVALIDARG };
    if each_container(pr, &n, true, ctx, cb) == 0 {
        return E_GS_CONTAINER_NOT_FOUND;
    }
    S_OK
}

pub unsafe extern "system" fn XGameSaveEnumerateContainerInfo(p: *mut Provider, ctx: *mut c_void, cb: Option<ContainerCb>) -> HRESULT {
    let (Some(pr), Some(cb)) = (provider(p), cb) else { return E_INVALIDARG };
    each_container(pr, "", false, ctx, cb);
    S_OK
}

pub unsafe extern "system" fn XGameSaveEnumerateContainerInfoByName(p: *mut Provider, prefix: *const u8, ctx: *mut c_void, cb: Option<ContainerCb>) -> HRESULT {
    let (Some(pr), Some(cb)) = (provider(p), cb) else { return E_INVALIDARG };
    each_container(pr, &from_cstr_ptr(prefix).unwrap_or_default(), false, ctx, cb);
    S_OK
}

unsafe fn each_blob(c: &Container, prefix: &str, ctx: *mut c_void, cb: BlobCb) {
    for (name, _, size) in blobs_of(&c.dir) {
        if !name.starts_with(prefix) {
            continue;
        }
        let cn = CString::new(name).unwrap_or_default();
        if !cb(&BlobInfo { name: cn.as_ptr() as *const u8, size: size as u32 }, ctx) {
            break;
        }
    }
}

pub unsafe extern "system" fn XGameSaveEnumerateBlobInfo(c: *mut Container, ctx: *mut c_void, cb: Option<BlobCb>) -> HRESULT {
    let (Some(c), Some(cb)) = (container(c), cb) else { return E_INVALIDARG };
    each_blob(c, "", ctx, cb);
    S_OK
}

pub unsafe extern "system" fn XGameSaveEnumerateBlobInfoByName(c: *mut Container, prefix: *const u8, ctx: *mut c_void, cb: Option<BlobCb>) -> HRESULT {
    let (Some(c), Some(cb)) = (container(c), cb) else { return E_INVALIDARG };
    each_blob(c, &from_cstr_ptr(prefix).unwrap_or_default(), ctx, cb);
    S_OK
}

/// Blobs `names` (all when empty) laid out as the API returns them: `Blob` array, then the
/// names and data; pointers stored as offsets from the start (fixed up by the caller).
fn read_layout(dir: &Path, names: &[String]) -> Result<(Vec<u8>, u32), HRESULT> {
    let all = blobs_of(dir);
    let picked: Vec<(String, PathBuf)> = if names.is_empty() {
        all.into_iter().map(|b| (b.0, b.1)).collect()
    } else {
        names
            .iter()
            .map(|n| all.iter().find(|b| &b.0 == n).map(|b| (b.0.clone(), b.1.clone())).ok_or(E_GS_BLOB_NOT_FOUND))
            .collect::<Result<_, _>>()?
    };
    let head = picked.len() * std::mem::size_of::<Blob>();
    let mut out = vec![0u8; head];
    for (k, (name, path)) in picked.iter().enumerate() {
        let data = std::fs::read(path).map_err(|_| E_GS_BLOB_NOT_FOUND)?;
        let name_at = out.len();
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        let data_at = out.len();
        out.extend_from_slice(&data);
        let e = k * std::mem::size_of::<Blob>();
        out[e..e + 8].copy_from_slice(&(name_at as u64).to_le_bytes());
        out[e + 8..e + 12].copy_from_slice(&(data.len() as u32).to_le_bytes());
        out[e + 16..e + 24].copy_from_slice(&(data_at as u64).to_le_bytes());
    }
    Ok((out, picked.len() as u32))
}

unsafe fn names_of(names: *const *const u8, count: u32) -> Result<Vec<String>, HRESULT> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if names.is_null() {
        return Err(E_INVALIDARG);
    }
    (0..count as usize).map(|i| from_cstr_ptr(*names.add(i)).ok_or(E_INVALIDARG)).collect()
}

/// Turn the offsets of a copied layout into pointers.
unsafe fn fix_up(buf: *mut Blob, count: u32) {
    for k in 0..count as usize {
        let b = &mut *buf.add(k);
        b.info.name = (buf as *const u8).add(b.info.name as usize);
        b.data = (buf as *mut u8).add(b.data as usize);
    }
}

pub unsafe extern "system" fn XGameSaveReadBlobData(c: *mut Container, names: *const *const u8, count: *mut u32, size: usize, out: *mut Blob) -> HRESULT {
    let Some(c) = container(c) else { return E_INVALIDARG };
    if count.is_null() {
        return E_INVALIDARG;
    }
    let requested = match names_of(names, *count) {
        Ok(names) => names,
        Err(e) => return e,
    };
    let (bytes, n) = match read_layout(&c.dir, &requested) {
        Ok(x) => x,
        Err(e) => return e,
    };
    if out.is_null() || size < bytes.len() {
        return E_GS_PROVIDED_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, bytes.len());
    fix_up(out, n);
    *count = n;
    S_OK
}

static READ_ID: u8 = 0;

pub unsafe extern "system" fn XGameSaveReadBlobDataAsync(c: *mut Container, names: *const *const u8, count: u32, b: *mut XAsyncBlock) -> HRESULT {
    let Some(c) = container(c) else { return E_INVALIDARG };
    let Ok(list) = names_of(names, count) else { return E_INVALIDARG };
    let dir = c.dir.clone();
    begin_with(b, &READ_ID as *const u8 as usize, move || {
        let (mut bytes, n) = read_layout(&dir, &list)?;
        bytes.splice(0..0, n.to_le_bytes()); // count travels in front, dropped by the Result
        Ok(bytes)
    })
}

pub unsafe extern "system" fn XGameSaveReadBlobDataResult(b: *mut XAsyncBlock, size: usize, out: *mut Blob, count: *mut u32) -> HRESULT {
    let mut need = 0usize;
    let hr = XAsyncGetResultSize(b, &mut need);
    if hr < 0 {
        return hr;
    }
    if need < std::mem::size_of::<u32>() {
        return E_FAIL;
    }
    let body_size = need - std::mem::size_of::<u32>();
    if out.is_null() || size < body_size {
        // Do not consume the one-shot XAsync result until the caller can
        // receive it; the documented insufficient-buffer retry must succeed.
        return E_GS_PROVIDED_BUFFER_TOO_SMALL;
    }
    let mut tmp = vec![0u8; need];
    let hr = XAsyncGetResult(b, &READ_ID as *const u8 as *const c_void, need, tmp.as_mut_ptr() as *mut c_void, std::ptr::null_mut());
    if hr < 0 {
        return hr;
    }
    let n = u32::from_le_bytes(tmp[..4].try_into().unwrap());
    let body = &tmp[4..];
    debug_assert_eq!(body.len(), body_size);
    std::ptr::copy_nonoverlapping(body.as_ptr(), out as *mut u8, body.len());
    fix_up(out, n);
    if !count.is_null() {
        *count = n;
    }
    S_OK
}

pub unsafe extern "system" fn XGameSaveCreateUpdate(c: *mut Container, display: *const u8, out: *mut *mut Update) -> HRESULT {
    let Some(c) = container(c) else { return E_INVALIDARG };
    if out.is_null() {
        return E_INVALIDARG;
    }
    let _ = &c.name;
    *out = Box::into_raw(Box::new(Update { magic: MAGIC_UPDATE, dir: c.dir.clone(), display: from_cstr_ptr(display), ops: Vec::new() }));
    S_OK
}

pub unsafe extern "system" fn XGameSaveCloseUpdate(u: *mut Update) {
    if u.as_ref().is_some_and(|u| u.magic == MAGIC_UPDATE) {
        (*u).magic = 0;
        drop(Box::from_raw(u));
    }
}

unsafe fn update<'a>(u: *mut Update) -> Option<&'a mut Update> {
    u.as_mut().filter(|u| u.magic == MAGIC_UPDATE)
}

pub unsafe extern "system" fn XGameSaveSubmitBlobWrite(u: *mut Update, name: *const u8, data: *const u8, size: usize) -> HRESULT {
    let (Some(u), Some(n)) = (update(u), from_cstr_ptr(name)) else { return E_INVALIDARG };
    if data.is_null() && size > 0 {
        return E_INVALIDARG;
    }
    let bytes = if size == 0 { Vec::new() } else { std::slice::from_raw_parts(data, size).to_vec() };
    u.ops.push((n, Some(bytes)));
    S_OK
}

pub unsafe extern "system" fn XGameSaveSubmitBlobDelete(u: *mut Update, name: *const u8) -> HRESULT {
    let (Some(u), Some(n)) = (update(u), from_cstr_ptr(name)) else { return E_INVALIDARG };
    u.ops.push((n, None));
    S_OK
}

/// Move `src` over `dst` atomically (they are always in the same directory here, so the
/// move cannot cross volumes). Windows `std::fs::rename` fails when `dst` already exists;
/// `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` replaces it, `MOVEFILE_WRITE_THROUGH`
/// has the new data on disk before the call returns.
#[cfg(windows)]
fn replace_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    let s: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let d: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(s.as_ptr(), d.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// POSIX `rename` already replaces an existing destination atomically.
#[cfg(not(windows))]
fn replace_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::rename(src, dst)
}

static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn apply(dir: &Path, display: Option<&str>, ops: &[(String, Option<Vec<u8>>)]) -> HRESULT {
    let blobs = dir.join("b");
    if std::fs::create_dir_all(&blobs).is_err() {
        return crate::util::E_FAIL;
    }
    if let Some(d) = display {
        if std::fs::write(dir.join("display"), d).is_err() {
            return crate::util::E_FAIL;
        }
    }
    for (name, data) in ops {
        let file = enc(name);
        let path = blobs.join(&file);
        match data {
            Some(d) => {
                // Write next to it, then move it over the destination in one step: a
                // crash never leaves half a blob, and an existing blob is replaced. The
                // temporary name appends to the *whole* encoded filename
                // (`with_extension` would drop the part after the last '.', so "a" and
                // "a.b" would share one temporary name). A literal '%' is encoded as
                // "%25", so the non-hex "%tmp" marker cannot occur in a blob filename.
                // The sequence also prevents concurrent updates from sharing a temp file.
                let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let tmp = blobs.join(format!("{file}.olympus%tmp.{}.{seq}", std::process::id()));
                if std::fs::write(&tmp, d).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                    return crate::util::E_FAIL;
                }
                if replace_file(&tmp, &path).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                    return crate::util::E_FAIL;
                }
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    S_OK
}

pub unsafe extern "system" fn XGameSaveSubmitUpdate(u: *mut Update) -> HRESULT {
    let Some(u) = update(u) else { return E_INVALIDARG };
    apply(&u.dir, u.display.as_deref(), &u.ops)
}

static SUBMIT_ID: u8 = 0;

pub unsafe extern "system" fn XGameSaveSubmitUpdateAsync(u: *mut Update, b: *mut XAsyncBlock) -> HRESULT {
    let Some(u) = update(u) else { return E_INVALIDARG };
    let (dir, display, ops) = (u.dir.clone(), u.display.clone(), u.ops.clone());
    begin_with(b, &SUBMIT_ID as *const u8 as usize, move || match apply(&dir, display.as_deref(), &ops) {
        S_OK => Ok(Vec::new()),
        e => Err(e),
    })
}

pub unsafe extern "system" fn XGameSaveSubmitUpdateResult(b: *mut XAsyncBlock) -> HRESULT {
    XAsyncGetResult(b, &SUBMIT_ID as *const u8 as *const c_void, 0, std::ptr::null_mut(), std::ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "system" fn count_containers(i: *const ContainerInfo, ctx: *mut c_void) -> bool {
        let v = &mut *(ctx as *mut Vec<(String, String, u32, u64)>);
        let i = &*i;
        v.push((from_cstr_ptr(i.name).unwrap(), from_cstr_ptr(i.display_name).unwrap(), i.blob_count, i.total_size));
        true
    }

    #[test]
    fn read_blob_names_require_a_complete_counted_array() {
        unsafe {
            let mut provider = std::ptr::null_mut();
            assert_eq!(
                XGameSaveInitializeProvider(
                    std::ptr::null_mut(),
                    b"olympus-read-name-validation\0".as_ptr(),
                    false,
                    &mut provider,
                ),
                S_OK
            );
            let _ = XGameSaveDeleteContainer(provider, b"slot\0".as_ptr());
            let mut container = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(provider, b"slot\0".as_ptr(), &mut container), S_OK);
            let mut update = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(container, std::ptr::null(), &mut update), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(update, b"data\0".as_ptr(), b"secret".as_ptr(), 6), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(update), S_OK);
            XGameSaveCloseUpdate(update);

            let mut count = 1u32;
            let mut buffer = vec![0u64; 32];
            assert_eq!(
                XGameSaveReadBlobData(container, std::ptr::null(), &mut count, 256, buffer.as_mut_ptr() as *mut Blob),
                E_INVALIDARG
            );
            assert_eq!(count, 1, "invalid filter changed the caller's count");
            let null_name = [std::ptr::null()];
            assert_eq!(
                XGameSaveReadBlobData(container, null_name.as_ptr(), &mut count, 256, buffer.as_mut_ptr() as *mut Blob),
                E_INVALIDARG
            );

            let mut async_block: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XGameSaveReadBlobDataAsync(container, std::ptr::null(), 1, &mut async_block), E_INVALIDARG);
            assert_eq!(XAsyncGetStatus(&mut async_block, false), E_INVALIDARG, "invalid async filter started an operation");
            assert_eq!(XGameSaveReadBlobDataAsync(container, null_name.as_ptr(), 1, &mut async_block), E_INVALIDARG);

            // The documented empty filter remains the read-all form.
            count = 0;
            assert_eq!(
                XGameSaveReadBlobData(container, std::ptr::null(), &mut count, 256, buffer.as_mut_ptr() as *mut Blob),
                S_OK
            );
            assert_eq!(count, 1);

            XGameSaveCloseContainer(container);
            assert_eq!(XGameSaveDeleteContainer(provider, b"slot\0".as_ptr()), S_OK);
            XGameSaveCloseProvider(provider);
        }
    }

    #[test]
    fn containers_blobs_round_trip() {
    let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let cfg = format!("xcl-test-{}\0", std::process::id());
            let mut p = std::ptr::null_mut();
            assert_eq!(XGameSaveInitializeProvider(std::ptr::null_mut(), cfg.as_ptr(), false, &mut p), S_OK);
            let mut c = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(p, b"slot/1\0".as_ptr(), &mut c), S_OK);
            let mut u = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, b"Save 1\0".as_ptr(), &mut u), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"data\0".as_ptr(), b"hello".as_ptr(), 5), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"meta\0".as_ptr(), b"xy".as_ptr(), 2), S_OK);
            // async submit
            let mut blk: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XGameSaveSubmitUpdateAsync(u, &mut blk), S_OK);
            assert_eq!(XAsyncGetStatus(&mut blk, true), S_OK);
            assert_eq!(XGameSaveSubmitUpdateResult(&mut blk), S_OK);
            XGameSaveCloseUpdate(u);
            let mut list: Vec<(String, String, u32, u64)> = Vec::new();
            assert_eq!(XGameSaveEnumerateContainerInfo(p, &mut list as *mut _ as *mut c_void, Some(count_containers)), S_OK);
            assert_eq!(list, vec![("slot/1".to_string(), "Save 1".to_string(), 2, 7)]);
            // read everything (sync): too small first
            let mut n = 0u32;
            let mut buf = vec![0u64; 32];
            assert_eq!(XGameSaveReadBlobData(c, std::ptr::null(), &mut n, 8, buf.as_mut_ptr() as *mut Blob), E_GS_PROVIDED_BUFFER_TOO_SMALL);
            n = 0;
            assert_eq!(XGameSaveReadBlobData(c, std::ptr::null(), &mut n, 256, buf.as_mut_ptr() as *mut Blob), S_OK);
            assert_eq!(n, 2);
            let b0 = &*(buf.as_ptr() as *const Blob);
            assert_eq!((from_cstr_ptr(b0.info.name).unwrap(), b0.info.size), ("data".to_string(), 5));
            assert_eq!(std::slice::from_raw_parts(b0.data, 5), b"hello");
            // read one by name (async), then delete it
            let names = [b"meta\0".as_ptr()];
            let mut blk: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XGameSaveReadBlobDataAsync(c, names.as_ptr(), 1, &mut blk), S_OK);
            assert_eq!(XAsyncGetStatus(&mut blk, true), S_OK);
            let mut buf2 = vec![0u64; 16];
            let mut n2 = u32::MAX;
            assert_eq!(
                XGameSaveReadBlobDataResult(&mut blk, 1, buf2.as_mut_ptr() as *mut Blob, &mut n2),
                E_GS_PROVIDED_BUFFER_TOO_SMALL
            );
            assert_eq!(n2, u32::MAX, "failed result changed the output count");
            assert_eq!(XGameSaveReadBlobDataResult(&mut blk, 128, buf2.as_mut_ptr() as *mut Blob, &mut n2), S_OK);
            let b1 = &*(buf2.as_ptr() as *const Blob);
            assert_eq!((n2, std::slice::from_raw_parts(b1.data, 2)), (1, &b"xy"[..]));
            let mut u = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, std::ptr::null(), &mut u), S_OK);
            assert_eq!(XGameSaveSubmitBlobDelete(u, b"meta\0".as_ptr()), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(u), S_OK);
            XGameSaveCloseUpdate(u);
            let names = [b"meta\0".as_ptr()];
            let mut n3 = 1u32;
            assert_eq!(XGameSaveReadBlobData(c, names.as_ptr(), &mut n3, 256, buf.as_mut_ptr() as *mut Blob), E_GS_BLOB_NOT_FOUND);
            let mut q = 0i64;
            assert_eq!(XGameSaveGetRemainingQuota(p, &mut q), S_OK);
            assert_eq!(q, QUOTA - 5);
            XGameSaveCloseContainer(c);
            assert_eq!(XGameSaveDeleteContainer(p, b"slot/1\0".as_ptr()), S_OK);
            assert_eq!(XGameSaveGetContainerInfo(p, b"slot/1\0".as_ptr(), std::ptr::null_mut(), Some(count_containers)), E_GS_CONTAINER_NOT_FOUND);
            let root = (*p).root.clone();
            XGameSaveCloseProvider(p);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    /// Names in `dir` still carrying the temporary-file suffix.
    fn temp_files(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir.join("b"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".olympus%tmp."))
            .collect()
    }

    #[test]
    fn apply_overwrites_an_existing_blob_repeatedly() {
        let dir = std::env::temp_dir().join(format!("olympus_gamesave_rewrite_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // first write, then several overwrites: on Windows std::fs::rename cannot replace
        // an existing destination, so every update after the first used to fail there
        assert_eq!(apply(&dir, Some("Save A"), &[("data".into(), Some(b"save one".to_vec()))]), S_OK);
        assert_eq!(std::fs::read(dir.join("b").join("data")).unwrap(), b"save one");
        for i in 2..=4 {
            let d = format!("save number {i}").into_bytes();
            assert_eq!(apply(&dir, None, &[("data".into(), Some(d.clone()))]), S_OK);
            assert_eq!(std::fs::read(dir.join("b").join("data")).unwrap(), d);
        }
        assert_eq!(std::fs::read_to_string(dir.join("display")).unwrap(), "Save A");
        // a successful update leaves no temporary files behind
        assert_eq!(temp_files(&dir), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_similar_names_never_collide_and_failures_clean_up() {
        let dir = std::env::temp_dir().join(format!("olympus_gamesave_names_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // names that share everything before the last '.' used to share one temporary file
        let ops: Vec<(String, Option<Vec<u8>>)> = [
            ("data", b"base".as_slice()),
            ("data.b", b"one".as_slice()),
            ("data.b.c", b"two".as_slice()),
        ]
        .into_iter()
        .map(|(n, d)| (n.to_string(), Some(d.to_vec())))
        .collect();
        assert_eq!(apply(&dir, None, &ops), S_OK);
        for (n, want) in [
            ("data", b"base".as_slice()),
            ("data.b", b"one".as_slice()),
            ("data.b.c", b"two".as_slice()),
        ] {
            assert_eq!(std::fs::read(dir.join("b").join(n)).unwrap(), want);
        }
        // overwriting just the middle name does not disturb the neighbours
        assert_eq!(apply(&dir, None, &[("data.b".into(), Some(b"ONE".to_vec()))]), S_OK);
        for (n, want) in [
            ("data", b"base".as_slice()),
            ("data.b", b"ONE".as_slice()),
            ("data.b.c", b"two".as_slice()),
        ] {
            assert_eq!(std::fs::read(dir.join("b").join(n)).unwrap(), want);
        }
        assert_eq!(temp_files(&dir), Vec::<String>::new());
        // a failed replacement (a directory sits at the blob path) removes its temp file
        std::fs::create_dir(dir.join("b").join("blocked")).unwrap();
        assert_eq!(apply(&dir, None, &[("blocked".into(), Some(b"x".to_vec()))]), crate::util::E_FAIL);
        assert!(dir.join("b").join("blocked").is_dir(), "a failed write must not disturb the destination");
        assert_eq!(temp_files(&dir), Vec::<String>::new());
        // a failed display-file write reports the error instead of silently dropping it
        std::fs::create_dir(dir.join("display")).unwrap();
        assert_eq!(apply(&dir, Some("blocked"), &[("nope".into(), Some(b"v".to_vec()))]), crate::util::E_FAIL);
        assert!(!dir.join("b").join("nope").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// enc() must put every non-empty name into exactly one safe file-name
    /// component: dec(enc(n)) == n, no two names share a file, and ".", "..",
    /// trailing '.'/' ' or reserved device stems can never alias a special path
    /// or another name on Windows.
    #[test]
    fn names_round_trip_stay_one_component_and_never_collide() {
        let names = [
            // ordinary names - their on-disk form must not change
            "default", "slot/1", "Save A", "data", "data.b", "data.b.c", "meta", "real_container",
            // path components and every trailing '.'/' ' Windows would strip
            ".", "..", "...", "....", ".a", "a.", "..a", "a..", " ", "  ", ". ", " .", ". .", ".. ", ".. .", "x ", "x.", "x. ",
            "x .",
            // reserved device basenames, bare or with extensions
            "CON", "con", "CoN", "PRN", "prn.ini", "AUX", "aux2", "NUL", "nul.log", "CLOCK$", "clock$.x", "COM1", "com9", "com0",
            "com10", "LPT1", "lpt9", "con.txt", "con.b.c", "con .x", "NUL ", "NUL .x",
            // escapes, separators and control bytes
            "%", "%%", "%2E", "%2e", "%25", "a%2F", ".%2E", "..%2E", "a/b", "a\\b", "a:b", "a?b", "a\"b", "a<b", "a>b", "a|b",
            "a\tb", "a\nb", "a\0b",
            // non-ASCII UTF-8
            "säve", "日本語", "a\u{1F600}b",
        ];
        let mut encoded = std::collections::HashMap::new();
        for n in names {
            let e = enc(n);
            assert!(
                !e.is_empty() && e != "." && e != ".." && !e.contains(['/', '\\']) && !e.ends_with(['.', ' ']),
                "{n:?} encoded to {e:?} - not one safe file-name component"
            );
            assert_eq!(dec(&e), n, "{n:?} -> {e:?} failed to decode back");
            if let Some(other) = encoded.insert(e.clone(), n) {
                panic!("{n:?} and {other:?} both encode to {e:?}");
            }
        }
        assert_eq!(dec(&enc("")), "");
        // names that were already safe keep their on-disk form byte for byte
        for (n, e) in [
            ("default", "default"),
            ("slot/1", "slot%2F1"),
            ("data.b.c", "data.b.c"),
            ("Save A", "Save A"),
            ("%", "%25"),
            ("%2E", "%252E"),
        ] {
            assert_eq!(enc(n), e, "{n:?}");
        }
        // dangerous names become unmistakably different file names instead
        for (n, e) in [
            (".", "%2E"),
            ("..", ".%2E"),
            ("...", "..%2E"),
            ("x ", "x%20"),
            ("x.", "x%2E"),
            ("con.txt", "%63on.txt"),
            ("COM1", "%43OM1"),
            ("clock$", "%63lock%24"),
            ("NUL .x", "%4EUL .x"),
            ("a/b", "a%2Fb"),
        ] {
            assert_eq!(enc(n), e, "{n:?}");
        }
    }

    /// '..' and friends must never let XGameSaveDeleteContainer touch anything
    /// outside the provider root: before the enc() fix, deleting ".." removed
    /// the root's parent and every sibling of the save folder.
    #[test]
    fn delete_container_cannot_escape_the_provider_root() {
        let base = std::env::temp_dir().join(format!("olympus_gamesave_guard_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("provider");
        let sentinel = base.join("sentinel");
        std::fs::create_dir_all(sentinel.join("inner")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(sentinel.join("keep.txt"), b"keep").unwrap();
        let real = root.join("real_container");
        std::fs::create_dir_all(real.join("b")).unwrap();
        std::fs::write(real.join("b").join("data"), b"save").unwrap();
        unsafe {
            let p = Box::into_raw(Box::new(Provider { magic: MAGIC_PROVIDER, root: root.clone() }));
            for name in [b"..\0".as_ptr(), b".\0".as_ptr(), b"...\0".as_ptr(), b" \0".as_ptr(), b"con\0".as_ptr()] {
                assert_eq!(XGameSaveDeleteContainer(p, name), S_OK);
            }
            // an empty name is rejected before it can join to the root itself
            assert_eq!(XGameSaveDeleteContainer(p, b"\0".as_ptr()), E_GS_INVALID_CONTAINER_NAME);
            assert!(sentinel.join("inner").is_dir() && sentinel.join("keep.txt").is_file(), "'..' escaped the provider root");
            assert!(real.is_dir(), "a delete of '..' must not fall onto a sibling dir");
            // a container literally named ".." lives inside the root, enumerates
            // back under its own name and deletes cleanly
            let mut c = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(p, b"..\0".as_ptr(), &mut c), S_OK);
            let mut u = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, std::ptr::null(), &mut u), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"d\0".as_ptr(), b"1".as_ptr(), 1), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(u), S_OK);
            XGameSaveCloseUpdate(u);
            XGameSaveCloseContainer(c);
            assert!(root.join(".%2E").join("b").join("d").is_file());
            let mut list: Vec<(String, String, u32, u64)> = Vec::new();
            assert_eq!(XGameSaveEnumerateContainerInfo(p, &mut list as *mut _ as *mut c_void, Some(count_containers)), S_OK);
            assert!(list.iter().any(|e| e == &("..".to_string(), "..".to_string(), 1, 1)));
            assert_eq!(XGameSaveDeleteContainer(p, b"..\0".as_ptr()), S_OK);
            assert!(!root.join(".%2E").exists());
            assert!(sentinel.join("keep.txt").is_file() && real.is_dir());
            // ordinary deletes still work
            assert_eq!(XGameSaveDeleteContainer(p, b"real_container\0".as_ptr()), S_OK);
            assert!(!real.exists());
            XGameSaveCloseProvider(p);
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
