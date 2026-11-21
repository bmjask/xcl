//! XGameRuntime – offline implementation of the GDK system APIs a title needs to boot.
//! Signatures: public GDK reference (docs.xbox.com).

#![allow(non_snake_case)]

pub mod api;
pub mod gamesave;
pub mod gdk_table;
pub mod taskqueue;
pub mod xasync;

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::util::{write_cstr_out, E_INVALIDARG, E_NOT_FOUND, E_NOT_SUFFICIENT_BUFFER, E_NOT_SUPPORTED, HRESULT, S_OK};
use gamesave::*;
use taskqueue::*;
use api::{InitializeApiImpl, InitializeApiImplEx, InitializeApiImplEx2, QueryApiImpl, UninitializeApiImpl, XErrorReport};
use xasync::*;

pub fn exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![
        // the GDK thunk protocol (api.rs) – what titles actually call
        InitializeApiImpl,
        InitializeApiImplEx,
        InitializeApiImplEx2,
        UninitializeApiImpl,
        QueryApiImpl,
        XErrorReport,
        // runtime
        XGameRuntimeInitialize,
        XGameRuntimeInitializeWithOptions,
        XGameRuntimeUninitialize,
        XGameRuntimeIsFeatureAvailable,
        // task queue
        XTaskQueueCreate,
        XTaskQueueDuplicateHandle,
        XTaskQueueCloseHandle,
        XTaskQueueSubmitCallback,
        XTaskQueueSubmitDelayedCallback,
        XTaskQueueDispatch,
        XTaskQueueIsEmpty,
        XTaskQueueTerminate,
        XTaskQueueGetCurrentProcessTaskQueue,
        XTaskQueueSetCurrentProcessTaskQueue,
        XTaskQueueRegisterWaiter,
        XTaskQueueUnregisterWaiter,
        XTaskQueueRegisterMonitor,
        XTaskQueueUnregisterMonitor,
        // async
        XAsyncBegin,
        XAsyncSchedule,
        XAsyncComplete,
        XAsyncGetStatus,
        XAsyncGetResultSize,
        XAsyncGetResult,
        XAsyncCancel,
        XAsyncRun,
        // threads
        XThreadSetTimeSensitive,
        XThreadIsTimeSensitive,
        // system
        XSystemGetDeviceType,
        XSystemGetConsoleId,
        XSystemGetAnalyticsInfo,
        XSystemGetXboxLiveSandboxId,
        // package
        XPackageIsPackagedProcess,
        XPackageGetCurrentProcessPackageIdentifier,
        XPackageMount,
        XPackageGetMountPathSize,
        XPackageGetMountPath,
        XPackageCloseMountHandle,
        XPackageEnumeratePackages,
        // storage
        XPersistentLocalStorageGetPathSize,
        XPersistentLocalStorageGetPath,
        XPersistentLocalStorageGetSpaceInfo,
        XGameSaveFilesGetFolderWithUiAsync,
        XGameSaveFilesGetFolderWithUiResult,
        // game save containers
        XGameSaveInitializeProvider,
        XGameSaveInitializeProviderAsync,
        XGameSaveInitializeProviderResult,
        XGameSaveCloseProvider,
        XGameSaveGetRemainingQuota,
        XGameSaveGetRemainingQuotaAsync,
        XGameSaveGetRemainingQuotaResult,
        XGameSaveDeleteContainer,
        XGameSaveDeleteContainerAsync,
        XGameSaveDeleteContainerResult,
        XGameSaveCreateContainer,
        XGameSaveCloseContainer,
        XGameSaveGetContainerInfo,
        XGameSaveEnumerateContainerInfo,
        XGameSaveEnumerateContainerInfoByName,
        XGameSaveEnumerateBlobInfo,
        XGameSaveEnumerateBlobInfoByName,
        XGameSaveReadBlobData,
        XGameSaveReadBlobDataAsync,
        XGameSaveReadBlobDataResult,
        XGameSaveCreateUpdate,
        XGameSaveCloseUpdate,
        XGameSaveSubmitBlobWrite,
        XGameSaveSubmitBlobDelete,
        XGameSaveSubmitUpdate,
        XGameSaveSubmitUpdateAsync,
        XGameSaveSubmitUpdateResult,
        // users
        XUserAddAsync,
        XUserAddResult,
        XUserDuplicateHandle,
        XUserCloseHandle,
        XUserCompare,
        XUserGetMaxUsers,
        XUserGetId,
        XUserGetLocalId,
        XUserFindUserByLocalId,
        XUserFindUserById,
        XUserGetIsGuest,
        XUserGetState,
        XUserGetGamertag,
        XUserGetAgeGroup,
        XUserRegisterForChangeEvent,
        XUserUnregisterForChangeEvent,
        // store (offline)
        XStoreCreateContext,
        XStoreCloseContextHandle,
    ]
}

// ---------------------------------------------------------------------------------------
// runtime
// ---------------------------------------------------------------------------------------

static INIT_COUNT: AtomicU32 = AtomicU32::new(0);

pub unsafe extern "system" fn XGameRuntimeInitialize() -> HRESULT {
    if INIT_COUNT.fetch_add(1, Ordering::AcqRel) == 0 {
        crate::xlog_info!("XGameRuntimeInitialize (XCL offline runtime)");
    }
    S_OK
}

pub unsafe extern "system" fn XGameRuntimeInitializeWithOptions(_options: *const c_void) -> HRESULT {
    XGameRuntimeInitialize()
}

pub unsafe extern "system" fn XGameRuntimeUninitialize() {
    INIT_COUNT.fetch_sub(1, Ordering::AcqRel);
}

pub unsafe extern "system" fn XGameRuntimeIsFeatureAvailable(feature: u32) -> bool {
    matches!(
        feature,
        2  // XAsync
            | 3  // XAsyncProvider
            | 5  // XGame
            | 7  // XGameSave
            | 11 // XPackage
            | 12 // XPersistentLocalStorage
            | 15 // XSystem
            | 16 // XTaskQueue
            | 17 // XThread
            | 18 // XUser
            | 19 // XError
    )
}

// ---------------------------------------------------------------------------------------
// XThread
// ---------------------------------------------------------------------------------------

thread_local! {
    static TIME_SENSITIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub unsafe extern "system" fn XThreadSetTimeSensitive(v: bool) -> HRESULT {
    TIME_SENSITIVE.with(|c| c.set(v));
    S_OK
}

pub unsafe extern "system" fn XThreadIsTimeSensitive() -> bool {
    TIME_SENSITIVE.with(|c| c.get())
}

// ---------------------------------------------------------------------------------------
// XSystem
// ---------------------------------------------------------------------------------------

pub const DEVICE_XBOX_SCARLETT_LOCKHART: u32 = 0x06;
pub const DEVICE_XBOX_SCARLETT_ANACONDA: u32 = 0x07;

pub unsafe extern "system" fn XSystemGetDeviceType() -> u32 {
    if crate::state::config().is_series_s() {
        DEVICE_XBOX_SCARLETT_LOCKHART
    } else {
        DEVICE_XBOX_SCARLETT_ANACONDA
    }
}

pub unsafe extern "system" fn XSystemGetConsoleId(size: usize, buf: *mut u8, used: *mut usize) -> HRESULT {
    write_cstr_out(&crate::state::config().console_id, buf, size, used)
}

pub unsafe extern "system" fn XSystemGetXboxLiveSandboxId(size: usize, buf: *mut u8, used: *mut usize) -> HRESULT {
    write_cstr_out(&crate::state::config().sandbox_id, buf, size, used)
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct XVersion {
    pub major: u16,
    pub minor: u16,
    pub build: u16,
    pub revision: u16,
}

#[repr(C)]
pub struct XSystemAnalyticsInfo {
    pub os_version: XVersion,
    pub hosting_os_version: XVersion,
    pub family: [u8; 64],
    pub form: [u8; 64],
}

fn fixed64(s: &str) -> [u8; 64] {
    let mut a = [0u8; 64];
    let b = s.as_bytes();
    a[..b.len().min(63)].copy_from_slice(&b[..b.len().min(63)]);
    a
}

/// What a console reports: the Xbox OS build, family "Windows.Xbox".
pub unsafe extern "system" fn XSystemGetAnalyticsInfo() -> XSystemAnalyticsInfo {
    let v = XVersion { major: 10, minor: 0, build: 25398, revision: 0 };
    let form = if crate::state::config().is_series_s() { "Xbox Series S" } else { "Xbox Series X" };
    XSystemAnalyticsInfo { os_version: v, hosting_os_version: v, family: fixed64("Windows.Xbox"), form: fixed64(form) }
}

// ---------------------------------------------------------------------------------------
// XPackage
// ---------------------------------------------------------------------------------------

pub fn package_identifier() -> String {
    let c = crate::state::config();
    if !c.package_identifier.is_empty() {
        return c.package_identifier.clone();
    }
    let title = crate::state::boot().map(|b| b.title.clone()).unwrap_or_else(|| "XCL.Title".into());
    format!("{title}_1.0.0.0_x64__xcl")
}

fn game_root() -> String {
    crate::state::boot()
        .map(|b| b.game_root.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into())
}

pub unsafe extern "system" fn XPackageIsPackagedProcess() -> bool {
    true
}

pub unsafe extern "system" fn XPackageGetCurrentProcessPackageIdentifier(size: usize, buf: *mut u8) -> HRESULT {
    write_cstr_out(&package_identifier(), buf, size, std::ptr::null_mut())
}

struct Mount {
    path: String,
}

pub unsafe extern "system" fn XPackageMount(id: *const u8, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let id = crate::util::from_cstr_ptr(id).unwrap_or_default();
    if !id.is_empty() && !id.eq_ignore_ascii_case(&package_identifier()) {
        crate::xlog_warn!("XPackageMount({id}) – unknown package (DLC?)");
        return E_NOT_FOUND;
    }
    *out = Box::into_raw(Box::new(Mount { path: game_root() })) as *mut c_void;
    S_OK
}

pub unsafe extern "system" fn XPackageGetMountPathSize(m: *mut c_void, size: *mut usize) -> HRESULT {
    if m.is_null() || size.is_null() {
        return E_INVALIDARG;
    }
    let mount = &*(m as *const Mount);
    *size = mount.path.len() + 1;
    S_OK
}

pub unsafe extern "system" fn XPackageGetMountPath(m: *mut c_void, size: usize, buf: *mut u8) -> HRESULT {
    if m.is_null() {
        return E_INVALIDARG;
    }
    write_cstr_out(&(*(m as *const Mount)).path, buf, size, std::ptr::null_mut())
}

pub unsafe extern "system" fn XPackageCloseMountHandle(m: *mut c_void) {
    if !m.is_null() {
        drop(Box::from_raw(m as *mut Mount));
    }
}

pub unsafe extern "system" fn XPackageEnumeratePackages(_kind: u32, _scope: u32, _ctx: *mut c_void, _cb: usize) -> HRESULT {
    // No DLC/related packages offline.
    S_OK
}

// ---------------------------------------------------------------------------------------
// Persistent local storage + save files
// ---------------------------------------------------------------------------------------

fn data_subdir(name: &str) -> PathBuf {
    let p = crate::state::data_dir().join(name);
    let _ = std::fs::create_dir_all(&p);
    p
}

fn pls_path() -> String {
    data_subdir("pls").to_string_lossy().into_owned()
}

pub unsafe extern "system" fn XPersistentLocalStorageGetPathSize(size: *mut usize) -> HRESULT {
    if size.is_null() {
        return E_INVALIDARG;
    }
    *size = pls_path().len() + 1;
    S_OK
}

pub unsafe extern "system" fn XPersistentLocalStorageGetPath(size: usize, buf: *mut u8, used: *mut usize) -> HRESULT {
    write_cstr_out(&pls_path(), buf, size, used)
}

#[repr(C)]
pub struct XPersistentLocalStorageSpaceInfo {
    pub available_free_bytes: u64,
    pub total_free_bytes: u64,
    pub used_bytes: u64,
    pub total_bytes: u64,
}

/// Bytes the title occupies under `root`: the sizes of regular files, recursively.
/// Entries whose metadata can't be read are skipped; the total saturates instead of
/// overflowing. Links and other reparse points are never followed and never counted —
/// a link could point outside `root`, and only in-tree bytes belong to the quota.
/// (`DirEntry::file_type` on Windows reports every name-surrogate reparse point —
/// symlinks, junctions and mount points — as `is_symlink`, without an extra syscall
/// and without dereferencing it.)
fn pls_used_bytes(root: &Path) -> u64 {
    let mut used = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            } else if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                used = used.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
            }
        }
    }
    used
}

/// `total_bytes` is the title's quota; both free figures are the smaller of the host's
/// and the quota still unused (`quota − used`, saturating at zero free once the title
/// sits over quota). `used_bytes` stays truthful even when it exceeds the quota.
fn pls_space_info(quota: u64, used: u64, avail: u64, free: u64) -> XPersistentLocalStorageSpaceInfo {
    let remaining = quota.saturating_sub(used);
    XPersistentLocalStorageSpaceInfo {
        available_free_bytes: avail.min(remaining),
        total_free_bytes: free.min(remaining),
        used_bytes: used,
        total_bytes: quota,
    }
}

pub unsafe extern "system" fn XPersistentLocalStorageGetSpaceInfo(info: *mut XPersistentLocalStorageSpaceInfo) -> HRESULT {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    if info.is_null() {
        return E_INVALIDARG;
    }
    let pls = pls_path();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    let ok = GetDiskFreeSpaceExW(crate::util::wide(&pls).as_ptr(), &mut avail, &mut total, &mut free);
    // the title's quota (MicrosoftGame.Config <PersistentLocalStorage><SizeMB>, else 4 GiB)
    let quota = match crate::state::config().pls_mb {
        0 => 4096,
        mb => mb,
    }
    .saturating_mul(1024 * 1024);
    let used = pls_used_bytes(Path::new(&pls));
    if ok == 0 {
        // host figures unavailable – let the remaining quota bound free space alone
        avail = quota;
        free = quota;
    }
    *info = pls_space_info(quota, used, avail, free);
    S_OK
}

static SAVE_FILES_IDENTITY: u8 = 0;

pub unsafe extern "system" fn XGameSaveFilesGetFolderWithUiAsync(_user: *mut c_void, scid: *const u8, b: *mut XAsyncBlock) -> HRESULT {
    let scid = crate::util::from_cstr_ptr(scid).unwrap_or_else(|| "default".into());
    let safe: String = scid.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    begin_with(b, &SAVE_FILES_IDENTITY as *const u8 as usize, move || {
        let p = data_subdir("saves").join(if safe.is_empty() { "default".into() } else { safe });
        std::fs::create_dir_all(&p).map_err(|_| crate::util::E_FAIL)?;
        let mut v = p.to_string_lossy().into_owned().into_bytes();
        v.push(0);
        Ok(v)
    })
}

pub unsafe extern "system" fn XGameSaveFilesGetFolderWithUiResult(b: *mut XAsyncBlock, size: usize, buf: *mut u8) -> HRESULT {
    XAsyncGetResult(b, &SAVE_FILES_IDENTITY as *const u8 as *const c_void, size, buf as *mut c_void, std::ptr::null_mut())
}

// ---------------------------------------------------------------------------------------
// XUser – one signed-in offline user
// ---------------------------------------------------------------------------------------

#[repr(C)]
pub struct User {
    magic: u32,
    refs: AtomicU32,
}

static USER: User = User { magic: 0x5553_4552, refs: AtomicU32::new(1) };
const LOCAL_ID: u64 = 1;
static USER_ADD_IDENTITY: u8 = 0;

fn user_ptr() -> *mut c_void {
    &USER as *const User as *mut c_void
}

unsafe fn valid_user(h: *mut c_void) -> bool {
    h == user_ptr()
}

pub unsafe extern "system" fn XUserAddAsync(options: u32, b: *mut XAsyncBlock) -> HRESULT {
    crate::xlog_info!("XUserAddAsync(options={options:#x}) → local offline user '{}'", crate::state::config().gamertag);
    begin_with(b, &USER_ADD_IDENTITY as *const u8 as usize, || {
        USER.refs.fetch_add(1, Ordering::AcqRel);
        Ok((user_ptr() as usize).to_le_bytes().to_vec())
    })
}

pub unsafe extern "system" fn XUserAddResult(b: *mut XAsyncBlock, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    XAsyncGetResult(b, &USER_ADD_IDENTITY as *const u8 as *const c_void, 8, out as *mut c_void, std::ptr::null_mut())
}

pub unsafe extern "system" fn XUserDuplicateHandle(h: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    USER.refs.fetch_add(1, Ordering::AcqRel);
    *out = h;
    S_OK
}

pub unsafe extern "system" fn XUserCloseHandle(h: *mut c_void) {
    if valid_user(h) {
        let _ = USER.refs.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| Some(v.saturating_sub(1).max(1)));
    }
}

pub unsafe extern "system" fn XUserCompare(a: *mut c_void, b: *mut c_void) -> i32 {
    (a as isize - b as isize).signum() as i32
}

pub unsafe extern "system" fn XUserGetMaxUsers(out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 1;
    S_OK
}

pub unsafe extern "system" fn XUserGetId(h: *mut c_void, out: *mut u64) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = crate::state::config().user_id;
    S_OK
}

pub unsafe extern "system" fn XUserGetLocalId(h: *mut c_void, out: *mut u64) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = LOCAL_ID;
    S_OK
}

pub unsafe extern "system" fn XUserFindUserByLocalId(id: u64, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    if id != LOCAL_ID {
        return E_NOT_FOUND;
    }
    USER.refs.fetch_add(1, Ordering::AcqRel);
    *out = user_ptr();
    S_OK
}

pub unsafe extern "system" fn XUserFindUserById(id: u64, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    if id != crate::state::config().user_id {
        return E_NOT_FOUND;
    }
    USER.refs.fetch_add(1, Ordering::AcqRel);
    *out = user_ptr();
    S_OK
}

pub unsafe extern "system" fn XUserGetIsGuest(h: *mut c_void, out: *mut bool) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = false;
    S_OK
}

pub unsafe extern "system" fn XUserGetState(h: *mut c_void, out: *mut u32) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = 0; // SignedIn
    S_OK
}

pub const GAMERTAG_SIZES: [usize; 4] = [16, 97, 15, 101]; // Classic, Modern, ModernSuffix, UniqueModern

pub unsafe extern "system" fn XUserGetGamertag(h: *mut c_void, component: u32, size: usize, buf: *mut u8, used: *mut usize) -> HRESULT {
    if !valid_user(h) || component > 3 {
        return E_INVALIDARG;
    }
    let tag = crate::state::config().gamertag.clone();
    let value = match component {
        2 => String::new(), // ModernSuffix: none
        3 => format!("{tag}#0001"),
        _ => tag,
    };
    if size < value.len() + 1 {
        if !used.is_null() {
            *used = value.len() + 1;
        }
        return E_NOT_SUFFICIENT_BUFFER;
    }
    write_cstr_out(&value, buf, size, used)
}

pub unsafe extern "system" fn XUserGetAgeGroup(h: *mut c_void, out: *mut u32) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = 3; // Adult
    S_OK
}

pub unsafe extern "system" fn XUserRegisterForChangeEvent(_q: *mut c_void, _ctx: *mut c_void, _cb: usize, token: *mut RegistrationToken) -> HRESULT {
    if token.is_null() {
        return E_INVALIDARG;
    }
    (*token).token = 1; // offline: the user never changes
    S_OK
}

pub unsafe extern "system" fn XUserUnregisterForChangeEvent(_token: RegistrationToken, _wait: bool) -> bool {
    true
}

// ---------------------------------------------------------------------------------------
// XStore – deliberately unavailable (no fake licences)
// ---------------------------------------------------------------------------------------

pub unsafe extern "system" fn XStoreCreateContext(_user: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    crate::xlog_warn!("XStoreCreateContext → unavailable (offline mode, XCL does not fake licenses)");
    if !out.is_null() {
        *out = std::ptr::null_mut();
    }
    E_NOT_SUPPORTED
}

pub unsafe extern "system" fn XStoreCloseContextHandle(_h: *mut c_void) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_availability_reports_only_implemented_families() {
        let expected = [2, 3, 5, 7, 11, 12, 15, 16, 17, 18, 19];
        for feature in 0..=21 {
            assert_eq!(
                unsafe { XGameRuntimeIsFeatureAvailable(feature) },
                expected.contains(&feature),
                "unexpected availability for public feature {feature}"
            );
        }
        assert!(!unsafe { XGameRuntimeIsFeatureAvailable(u32::MAX) });
    }

    #[test]
    fn user_add_and_queries() {
    let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XUserAddAsync(1, &mut b), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b, true), S_OK);
            let mut u = std::ptr::null_mut();
            assert_eq!(XUserAddResult(&mut b, &mut u), S_OK);
            assert!(!u.is_null());
            let mut tag = [0u8; 16];
            let mut used = 0usize;
            assert_eq!(XUserGetGamertag(u, 0, 16, tag.as_mut_ptr(), &mut used), S_OK);
            assert_eq!(&tag[..used - 1], b"Player1");
            assert_eq!(XUserGetGamertag(u, 0, 3, tag.as_mut_ptr(), &mut used), E_NOT_SUFFICIENT_BUFFER);
            let mut id = 0u64;
            assert_eq!(XUserGetId(u, &mut id), S_OK);
            assert_ne!(id, 0);
            XUserCloseHandle(u);
            assert_eq!(XUserGetId(std::ptr::null_mut(), &mut id), E_INVALIDARG);
        }
    }

    #[test]
    fn system_info() {
        unsafe {
            assert_eq!(XSystemGetDeviceType(), DEVICE_XBOX_SCARLETT_ANACONDA);
            let info = XSystemGetAnalyticsInfo();
            assert!(info.os_version.major >= 10);
            assert_eq!(&info.form[..13], b"Xbox Series X");
            assert_eq!(std::mem::size_of::<XSystemAnalyticsInfo>(), 144);
        }
    }

    #[test]
    fn save_folder_async() {
    let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XGameSaveFilesGetFolderWithUiAsync(std::ptr::null_mut(), b"scid-1\0".as_ptr(), &mut b), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b, true), S_OK);
            let mut buf = [0u8; 512];
            assert_eq!(XGameSaveFilesGetFolderWithUiResult(&mut b, 512, buf.as_mut_ptr()), S_OK);
            let s = std::ffi::CStr::from_ptr(buf.as_ptr() as *const _).to_string_lossy().into_owned();
            assert!(s.ends_with("saves\\scid-1"), "{s}");
            assert!(std::path::Path::new(&s).is_dir());
        }
    }

    fn pls_test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("olympus_pls_test_{}_{}", std::process::id(), name))
    }

    #[test]
    fn pls_used_counts_nested_files() {
        let root = pls_test_dir("nested");
        let _ = std::fs::remove_dir_all(&root);
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.join("top.bin"), [0u8; 100]).unwrap();
        std::fs::write(root.join("a").join("mid.bin"), [0u8; 50]).unwrap();
        std::fs::write(deep.join("deep.bin"), [0u8; 250]).unwrap();
        assert_eq!(pls_used_bytes(&root), 400);
        assert_eq!(pls_used_bytes(&root.join("missing")), 0); // unreadable → 0, not an error
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pls_space_math_quota_and_saturation() {
        const MIB: u64 = 1024 * 1024;
        // the quota is reduced by measured use when the host disk is not the limit
        let s = pls_space_info(4 * MIB, 300_000, u64::MAX, u64::MAX);
        assert_eq!((s.used_bytes, s.total_bytes), (300_000, 4 * MIB));
        assert_eq!(s.available_free_bytes, 4 * MIB - 300_000);
        assert_eq!(s.total_free_bytes, 4 * MIB - 300_000);
        // the host disk is the tighter limit
        let s = pls_space_info(4 * MIB, 0, 1234, 5678);
        assert_eq!((s.available_free_bytes, s.total_free_bytes), (1234, 5678));
        // over quota: free saturates at 0 while used stays truthful
        let s = pls_space_info(4 * MIB, 4 * MIB + 7, u64::MAX, u64::MAX);
        assert_eq!((s.available_free_bytes, s.total_free_bytes), (0, 0));
        assert_eq!(s.used_bytes, 4 * MIB + 7);
        // a huge configured quota never overflows or underflows
        let s = pls_space_info(u64::MAX, u64::MAX - 1, u64::MAX, u64::MAX);
        assert_eq!((s.available_free_bytes, s.total_free_bytes), (1, 1));
        let s = pls_space_info(10, 20, 0, u64::MAX);
        assert_eq!((s.available_free_bytes, s.total_free_bytes), (0, 0));
    }

    #[cfg(windows)]
    #[test]
    fn pls_used_does_not_follow_dir_links() {
        let base = pls_test_dir("links");
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("pls");
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("escape.bin"), [0u8; 777]).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("real.bin"), [0u8; 111]).unwrap();
        // a junction needs no elevation (unlike a symlink); skip when it can't be made
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(root.join("linked"))
            .arg(&outside)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            let _ = std::fs::remove_dir_all(&base);
            eprintln!("mklink /J unavailable – link case skipped");
            return;
        }
        assert_eq!(pls_used_bytes(&root), 111); // the junction and its target are not counted
        // nor is a file symlink's target, when it can be created (needs the privilege)
        if std::os::windows::fs::symlink_file(&outside.join("escape.bin"), &root.join("flink.bin")).is_ok() {
            assert_eq!(pls_used_bytes(&root), 111);
        }
        let _ = std::fs::remove_dir_all(&base); // remove_dir_all removes the link, not the target
    }

    #[test]
    fn pls_space_info_export() {
        unsafe {
            assert_eq!(XPersistentLocalStorageGetSpaceInfo(std::ptr::null_mut()), E_INVALIDARG);
            let mut info: XPersistentLocalStorageSpaceInfo = std::mem::zeroed();
            assert_eq!(XPersistentLocalStorageGetSpaceInfo(&mut info), S_OK);
            let before = info.used_bytes;
            let probe = data_subdir("pls").join(format!("probe_{}.bin", std::process::id()));
            std::fs::write(&probe, [0u8; 321]).unwrap();
            assert_eq!(XPersistentLocalStorageGetSpaceInfo(&mut info), S_OK);
            assert_eq!(info.used_bytes, before + 321);
            assert_eq!(info.used_bytes, pls_used_bytes(&data_subdir("pls")));
            assert_eq!(info.total_bytes, 4096u64 << 20); // default quota: 4 GiB
            assert!(info.available_free_bytes <= info.total_free_bytes);
            assert!(info.total_free_bytes + info.used_bytes <= info.total_bytes);
            let _ = std::fs::remove_file(&probe);
        }
    }
}
