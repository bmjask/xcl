//! Non-blocking pipeline creation. Translating a console shader takes milliseconds; compiling
//! the HLSL with DXC takes 0.2 s to seconds, and a title creates thousands of pipelines while
//! it loads. When a pipeline's DXIL is not in the disk cache yet, the title gets a stand-in
//! pipeline (shaders that draw nothing) at once and the real one is built on a worker; binding
//! (`SetPipelineState`, console list replay, the two-pass lookups) uses the real one as soon
//! as it is there. A graphics pipeline draws nothing until then; a compute pipeline is waited
//! for at its first bind (a skipped dispatch would corrupt what it computes).
//! `XCL_ASYNC_PSO=0` builds every pipeline in the call, as before.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use windows_sys::core::GUID;

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_D3D12_OBJECT: GUID = GUID::from_u128(0xc4fec28f_7966_4e95_9f94_f431cb56c3b8);
const IID_D3D12_DEVICE_CHILD: GUID = GUID::from_u128(0x905db94b_a00c_4140_9df5_2b64ca9ea357);
const IID_D3D12_PAGEABLE: GUID = GUID::from_u128(0x63ee58fb_1268_4835_86da_f008ce62f0d6);
const IID_D3D12_PIPELINE_STATE: GUID = GUID::from_u128(0x765a30f3_f624_4c6f_a828_ace948622445);

fn same_guid(a: GUID, b: GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

struct Pending {
    /// the real pipeline state (raw pointer), 0 while building or when building failed
    real: AtomicUsize,
    done: Mutex<bool>,
    cv: Condvar,
    compute: bool,
    /// The stand-in is XCL's unique COM-shaped token, not a driver pipeline.
    deferred: bool,
    /// the stand-in was released before the real pipeline was built
    orphan: std::sync::atomic::AtomicBool,
}

/// A unique pipeline-shaped token used when the driver cannot create even the dummy PSO.
/// It is never passed to D3D12. Binding resolves it to the real pipeline, or zero while the
/// background build is pending/failed.
#[repr(C)]
struct DeferredPso {
    vtbl: *const usize,
    refs: AtomicU32,
    device: usize,
    creation_iid: Option<GUID>,
}

unsafe impl Send for DeferredPso {}
unsafe impl Sync for DeferredPso {}

fn deferred_vtable() -> *const usize {
    static VTABLE: OnceLock<usize> = OnceLock::new();
    *VTABLE.get_or_init(|| {
        let mut v: Vec<usize> = (0..512)
            .map(|i| crate::stubs::make_stub_info("d3d12_xs.dll", &format!("ID3D12PipelineState(deferred)::#{i}"), 0).0)
            .collect();
        let known = [
            deferred_query_interface as usize,
            deferred_add_ref as usize,
            deferred_release as usize,
            deferred_get_private_data as usize,
            deferred_ok3 as usize,
            deferred_ok2 as usize,
            deferred_ok1 as usize,
            deferred_get_device as usize,
            deferred_get_cached_blob as usize,
        ];
        v[..known.len()].copy_from_slice(&known);
        Box::leak(v.into_boxed_slice()).as_ptr() as usize
    }) as *const usize
}

unsafe extern "system" fn deferred_query_interface(this: *mut DeferredPso, iid: *const c_void, out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return crate::util::E_POINTER;
    }
    *out = std::ptr::null_mut();
    if iid.is_null() {
        return crate::util::E_NOINTERFACE;
    }
    let iid = *(iid as *const GUID);
    let supported = [IID_IUNKNOWN, IID_D3D12_OBJECT, IID_D3D12_DEVICE_CHILD, IID_D3D12_PAGEABLE, IID_D3D12_PIPELINE_STATE]
        .into_iter()
        .any(|known| same_guid(iid, known))
        || (*this).creation_iid.is_some_and(|alias| same_guid(iid, alias));
    if !supported {
        return crate::util::E_NOINTERFACE;
    }
    deferred_add_ref(this);
    *out = this as *mut c_void;
    crate::util::S_OK
}

unsafe extern "system" fn deferred_add_ref(this: *mut DeferredPso) -> u32 {
    (*this).refs.fetch_add(1, Ordering::AcqRel) + 1
}

unsafe extern "system" fn deferred_release(this: *mut DeferredPso) -> u32 {
    let n = (*this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if n == 0 {
        crate::hle::d3d12x::real::forget_pso(this as usize);
        release(released(this as usize));
        com_release((*this).device);
        drop(Box::from_raw(this));
    }
    n
}

unsafe extern "system" fn deferred_get_private_data(_this: *mut DeferredPso, _guid: *const c_void, size: *mut u32, _data: *mut c_void) -> i32 {
    if !size.is_null() {
        *size = 0;
    }
    0x887A_0002u32 as i32 // DXGI_ERROR_NOT_FOUND
}

unsafe extern "system" fn deferred_ok3(_this: *mut DeferredPso, _a: usize, _b: usize, _c: usize) -> i32 {
    crate::util::S_OK
}

unsafe extern "system" fn deferred_ok2(_this: *mut DeferredPso, _a: usize, _b: usize) -> i32 {
    crate::util::S_OK
}

unsafe extern "system" fn deferred_ok1(_this: *mut DeferredPso, _a: usize) -> i32 {
    crate::util::S_OK
}

unsafe extern "system" fn deferred_get_device(this: *mut DeferredPso, iid: *const c_void, out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return crate::util::E_POINTER;
    }
    *out = std::ptr::null_mut();
    if (*this).device == 0 || iid.is_null() {
        return crate::util::E_NOINTERFACE;
    }
    let device = (*this).device as *mut c_void;
    let vtbl = *(device as *const *const usize);
    let query: unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void) -> i32 = std::mem::transmute(*vtbl);
    query(device, iid, out)
}

unsafe extern "system" fn deferred_get_cached_blob(_this: *mut DeferredPso, out: *mut *mut c_void) -> i32 {
    if !out.is_null() {
        *out = std::ptr::null_mut();
    }
    crate::util::E_NOTIMPL
}

fn deferred(device: usize, creation_iid: Option<GUID>) -> usize {
    com_add_ref(device);
    Box::into_raw(Box::new(DeferredPso { vtbl: deferred_vtable(), refs: AtomicU32::new(1), device, creation_iid })) as usize
}

fn com_add_ref(object: usize) {
    if object != 0 {
        unsafe {
            let vtbl = *(object as *const *const usize);
            let add_ref: unsafe extern "system" fn(usize) -> u32 = std::mem::transmute(*vtbl.add(1));
            add_ref(object);
        }
    }
}

fn com_release(object: usize) {
    if object != 0 {
        unsafe {
            let vtbl = *(object as *const *const usize);
            let release: unsafe extern "system" fn(usize) -> u32 = std::mem::transmute(*vtbl.add(2));
            release(object);
        }
    }
}

struct ComRef(usize);

unsafe impl Send for ComRef {}

impl ComRef {
    fn new(object: usize) -> Self {
        com_add_ref(object);
        Self(object)
    }
}

impl Drop for ComRef {
    fn drop(&mut self) {
        com_release(self.0);
    }
}

/// stand-in handed to the title → its real pipeline
static PENDING: Mutex<Option<HashMap<usize, Arc<Pending>>>> = Mutex::new(None);
static BUILT: AtomicUsize = AtomicUsize::new(0);
static FAILED: AtomicUsize = AtomicUsize::new(0);

pub fn enabled() -> bool {
    std::env::var("XCL_ASYNC_PSO").map_or(true, |v| v != "0")
}

/// Builds the real pipeline: its raw pointer, 0 = failed.
pub type Job = Box<dyn FnOnce() -> usize + Send>;

fn queue() -> &'static Mutex<mpsc::Sender<(Arc<Pending>, Job)>> {
    static Q: OnceLock<Mutex<mpsc::Sender<(Arc<Pending>, Job)>>> = OnceLock::new();
    Q.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<(Arc<Pending>, Job)>();
        let rx = Arc::new(Mutex::new(rx));
        // translation recurses deeply on big programs; DXC itself runs on its own pool
        let n = crate::gpu::recomp::dxc::compile_threads();
        for k in 0..n {
            let rx = rx.clone();
            let _ = std::thread::Builder::new().name(format!("xcl-pso-{k}")).stack_size(64 << 20).spawn(move || loop {
                let next = rx.lock().unwrap().recv();
                let Ok((p, job)) = next else { return };
                let real = job();
                if real == 0 {
                    FAILED.fetch_add(1, Ordering::Relaxed);
                } else {
                    BUILT.fetch_add(1, Ordering::Relaxed);
                }
                let orphan = {
                    let mut done = p.done.lock().unwrap();
                    let orphan = p.orphan.load(Ordering::Acquire);
                    if !orphan {
                        p.real.store(real, Ordering::Release);
                    }
                    *done = true;
                    p.cv.notify_all();
                    orphan
                };
                // outside `done`: the Release override takes PENDING (`counts` takes PENDING, then `done`)
                if orphan {
                    release(real);
                }
            });
        }
        Mutex::new(tx)
    })
}

/// Release a pipeline state XCL owns (raw pointer, 0 = none).
pub fn release(p: usize) {
    if p != 0 {
        unsafe { drop(<windows::Win32::Graphics::Direct3D12::ID3D12PipelineState as windows::core::Interface>::from_raw(p as *mut std::ffi::c_void)) };
    }
}

/// `standin` went to the title; `job` builds the real pipeline on a worker.
fn submit_inner(standin: usize, compute: bool, deferred: bool, job: Job) {
    let p = Arc::new(Pending { real: AtomicUsize::new(0), done: Mutex::new(false), cv: Condvar::new(), compute, deferred, orphan: Default::default() });
    PENDING.lock().unwrap().get_or_insert_with(HashMap::new).insert(standin, p.clone());
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        crate::xlog_info!("D3D12.X: pipelines whose shaders are not compiled yet get a stand-in and are built in the background");
    });
    if let Err(e) = queue().lock().unwrap().send((p, job)) {
        let (p, _job) = e.0;
        FAILED.fetch_add(1, Ordering::Relaxed);
        let mut done = p.done.lock().unwrap();
        *done = true;
        p.cv.notify_all();
    }
}

pub fn submit(standin: usize, compute: bool, job: Job) {
    submit_inner(standin, compute, false, job)
}

/// Make a unique non-driver stand-in and build its real pipeline in the background.
pub fn submit_deferred(device: usize, compute: bool, creation_iid: Option<GUID>, job: Job) -> usize {
    let standin = deferred(device, creation_iid);
    // The title may release its token while the worker is still using the device.
    let device_ref = ComRef::new(device);
    submit_inner(
        standin,
        compute,
        true,
        Box::new(move || {
            let _device_ref = device_ref;
            job()
        }),
    );
    standin
}

fn entry(pso: usize) -> Option<Arc<Pending>> {
    PENDING.lock().unwrap().as_ref()?.get(&pso).cloned()
}

/// The pipeline to use for `pso` now: the real one once built, else `pso` itself.
pub fn resolve(pso: usize) -> usize {
    match entry(pso).map(|p| (p.real.load(Ordering::Acquire), p.deferred)) {
        Some((r, _)) if r != 0 => r,
        Some((_, true)) => 0,
        _ => pso,
    }
}

/// `resolve`, waiting for a pipeline still being built.
pub fn resolve_wait(pso: usize) -> usize {
    let Some(p) = entry(pso) else { return pso };
    let mut done = p.done.lock().unwrap();
    while !*done {
        done = p.cv.wait(done).unwrap();
    }
    match (p.real.load(Ordering::Acquire), p.deferred) {
        (0, true) => 0,
        (0, false) => pso,
        (r, _) => r,
    }
}

/// For binding: compute pipelines are waited for, graphics ones are not.
pub fn resolve_for_bind(pso: usize) -> usize {
    match entry(pso) {
        Some(p) if p.compute => resolve_wait(pso),
        Some(_) => resolve(pso),
        None => pso,
    }
}

/// The title released stand-in `pso` for good: forget it (its address may come back for
/// another object) and return its real pipeline to release (0: none, or still being built –
/// the worker's result is then released when it arrives).
pub fn released(pso: usize) -> usize {
    let Some(p) = PENDING.lock().unwrap().as_mut().and_then(|m| m.remove(&pso)) else { return 0 };
    let done = p.done.lock().unwrap();
    if *done {
        p.real.swap(0, Ordering::AcqRel)
    } else {
        // the worker stores its result into a Pending nobody will read: mark it orphaned
        p.orphan.store(true, Ordering::Release);
        0
    }
}

/// `pso` is a stand-in whose real pipeline is still being built.
pub fn is_pending(pso: usize) -> bool {
    entry(pso).is_some_and(|p| !*p.done.lock().unwrap())
}

pub fn is_deferred(pso: usize) -> bool {
    entry(pso).is_some_and(|p| p.deferred)
}

/// (built in the background, failed, still building)
pub fn counts() -> (usize, usize, usize) {
    let pending = PENDING.lock().unwrap().as_ref().map_or(0, |m| m.values().filter(|p| !*p.done.lock().unwrap()).count());
    (BUILT.load(Ordering::Relaxed), FAILED.load(Ordering::Relaxed), pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct FakePso {
        vtbl: *const usize,
    }

    static FAKE_RELEASES: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "system" fn fake_query(_this: *mut FakePso, _iid: *const GUID, _out: *mut *mut c_void) -> i32 {
        crate::util::E_NOINTERFACE
    }

    unsafe extern "system" fn fake_add_ref(_this: *mut FakePso) -> u32 {
        2
    }

    unsafe extern "system" fn fake_release(this: *mut FakePso) -> u32 {
        FAKE_RELEASES.fetch_add(1, Ordering::AcqRel);
        drop(Box::from_raw(this));
        0
    }

    fn fake_pso() -> usize {
        static VTABLE: OnceLock<usize> = OnceLock::new();
        let vtbl = *VTABLE.get_or_init(|| Box::leak(Box::new([fake_query as usize, fake_add_ref as usize, fake_release as usize])).as_ptr() as usize);
        Box::into_raw(Box::new(FakePso { vtbl: vtbl as *const usize })) as usize
    }

    #[test]
    fn deferred_standins_are_unique_and_never_resolve_to_fake_driver_objects() {
        let a = submit_deferred(0, false, None, Box::new(|| 0));
        let b = submit_deferred(0, true, None, Box::new(|| 0));
        assert_ne!(a, b);
        assert!(is_deferred(a));
        assert_eq!(resolve(a), 0);
        assert_eq!(resolve_wait(a), 0);
        assert_eq!(resolve_for_bind(b), 0);
        unsafe {
            let release: unsafe extern "system" fn(*mut DeferredPso) -> u32 = std::mem::transmute(*(*(a as *const *const usize)).add(2));
            assert_eq!(release(a as *mut DeferredPso), 0);
            assert_eq!(release(b as *mut DeferredPso), 0);
        }
        assert!(!is_pending(a));
        assert!(!is_pending(b));
    }

    #[test]
    fn deferred_standin_resolves_to_completed_real_pipeline() {
        let real = 0x7f00_1234usize;
        let pso = submit_deferred(0, false, None, Box::new(move || real));
        assert_eq!(resolve_wait(pso), real);
        assert_eq!(released(pso), real);
        unsafe {
            let release: unsafe extern "system" fn(*mut DeferredPso) -> u32 = std::mem::transmute(*(*(pso as *const *const usize)).add(2));
            assert_eq!(release(pso as *mut DeferredPso), 0);
        }
    }

    #[test]
    fn pending_deferred_graphics_resolution_is_nonblocking() {
        let (allow, gate) = std::sync::mpsc::channel();
        let pso = submit_deferred(
            0,
            false,
            None,
            Box::new(move || {
                let _ = gate.recv();
                0
            }),
        );
        assert!(is_pending(pso));
        assert_eq!(resolve(pso), 0);
        assert_eq!(resolve_for_bind(pso), 0);
        allow.send(()).unwrap();
        assert_eq!(resolve_wait(pso), 0);
        unsafe {
            let release: unsafe extern "system" fn(*mut DeferredPso) -> u32 = std::mem::transmute(*(*(pso as *const *const usize)).add(2));
            assert_eq!(release(pso as *mut DeferredPso), 0);
        }
    }

    #[test]
    fn deferred_query_interface_obeys_com_identity_and_reference_counts() {
        let alias = GUID::from_u128(0x12345678_9abc_def0_1234_56789abcdef0);
        let pso = deferred(0, Some(alias));
        unsafe {
            let vtbl = *(pso as *const *const usize);
            let query: unsafe extern "system" fn(*mut DeferredPso, *const GUID, *mut *mut c_void) -> i32 = std::mem::transmute(*vtbl);
            let release: unsafe extern "system" fn(*mut DeferredPso) -> u32 = std::mem::transmute(*vtbl.add(2));

            let mut out = usize::MAX as *mut c_void;
            assert_eq!(query(pso as *mut DeferredPso, &IID_IUNKNOWN, &mut out), crate::util::S_OK);
            assert_eq!(out as usize, pso);
            assert_eq!(release(pso as *mut DeferredPso), 1, "QI owns a reference");

            out = usize::MAX as *mut c_void;
            assert_eq!(query(pso as *mut DeferredPso, &alias, &mut out), crate::util::S_OK);
            assert_eq!(out as usize, pso);
            assert_eq!(release(pso as *mut DeferredPso), 1);

            let unsupported = GUID::from_u128(0xfedcba98_7654_3210_fedc_ba9876543210);
            out = usize::MAX as *mut c_void;
            assert_eq!(query(pso as *mut DeferredPso, &unsupported, &mut out), crate::util::E_NOINTERFACE);
            assert!(out.is_null());
            assert_eq!(query(pso as *mut DeferredPso, &IID_IUNKNOWN, std::ptr::null_mut()), crate::util::E_POINTER);

            let get_device: unsafe extern "system" fn(*mut DeferredPso, *const GUID, *mut *mut c_void) -> i32 = std::mem::transmute(*vtbl.add(7));
            out = usize::MAX as *mut c_void;
            assert_eq!(get_device(pso as *mut DeferredPso, &IID_IUNKNOWN, &mut out), crate::util::E_NOINTERFACE);
            assert!(out.is_null());
            assert_eq!(get_device(pso as *mut DeferredPso, &IID_IUNKNOWN, std::ptr::null_mut()), crate::util::E_POINTER);
            assert_eq!(release(pso as *mut DeferredPso), 0);
        }
    }

    #[test]
    fn release_before_worker_completion_releases_real_pipeline_once() {
        FAKE_RELEASES.store(0, Ordering::Release);
        let (allow, gate) = std::sync::mpsc::channel();
        let real = fake_pso();
        let pso = submit_deferred(
            0,
            false,
            None,
            Box::new(move || {
                let _ = gate.recv();
                real
            }),
        );
        unsafe {
            let release: unsafe extern "system" fn(*mut DeferredPso) -> u32 = std::mem::transmute(*(*(pso as *const *const usize)).add(2));
            assert_eq!(release(pso as *mut DeferredPso), 0);
        }
        allow.send(()).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while FAKE_RELEASES.load(Ordering::Acquire) == 0 && std::time::Instant::now() < until {
            std::thread::yield_now();
        }
        assert_eq!(FAKE_RELEASES.load(Ordering::Acquire), 1);
    }
}
