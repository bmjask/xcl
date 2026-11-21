//! XAsync / XAsyncProvider (contract: libHttpClient XAsync.h + XAsyncProvider.h, MIT).
//!
//! `XAsyncBlock` = { queue, context, callback, internal[32] }. We keep a pointer to our
//! state in `internal[0..8]`.
//!
//! A title that reaches the runtime through the GDK thunks (`api.rs`) gets its task queues and
//! async blocks from the PC's XGameRuntime; then these functions – and so every XCL async API
//! (`begin_with`) – go to the PC runtime's XAsync object (`use_pc`) instead.

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};

use super::taskqueue::{self, Item, PORT_COMPLETION, PORT_WORK};
use crate::util::{E_ABORT, E_INVALIDARG, E_NOT_SUFFICIENT_BUFFER, E_PENDING, HRESULT, S_OK};

pub const OP_BEGIN: u32 = 0;
pub const OP_DO_WORK: u32 = 1;
pub const OP_GET_RESULT: u32 = 2;
pub const OP_CANCEL: u32 = 3;
pub const OP_CLEANUP: u32 = 4;

#[repr(C)]
pub struct XAsyncBlock {
    pub queue: *mut c_void,
    pub context: *mut c_void,
    pub callback: usize,
    pub internal: [u8; 32],
}

#[repr(C)]
pub struct XAsyncProviderData {
    pub async_: *mut XAsyncBlock,
    pub buffer_size: usize,
    pub buffer: *mut c_void,
    pub context: *mut c_void,
}

pub type XAsyncProvider = unsafe extern "system" fn(u32, *const XAsyncProviderData) -> HRESULT;

/// The PC runtime's XAsync / XTaskQueue object (0 = XCL's own queues). Its slots: 3 GetStatus,
/// 4 GetResultSize, 5 Cancel, 6 Run, 7 Begin, 9 Schedule, 10 Complete, 11 GetResult.
static PC_ASYNC: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// tests that use XCL's queues and the one that switches to the PC runtime take turns
#[cfg(test)]
pub static ASYNC_TEST_LOCK: Mutex<()> = Mutex::new(());

pub fn use_pc(object: usize) {
    PC_ASYNC.store(object, std::sync::atomic::Ordering::Release);
}

/// (object, slot function) of the PC XAsync object, when in use
unsafe fn pc(slot: usize) -> Option<(usize, usize)> {
    let o = PC_ASYNC.load(std::sync::atomic::Ordering::Acquire);
    (o != 0).then(|| (o, *(*(o as *const *const usize)).add(slot)))
}
pub type XAsyncWork = unsafe extern "system" fn(*mut XAsyncBlock) -> HRESULT;
type Completion = unsafe extern "system" fn(*mut XAsyncBlock);

struct Inner {
    status: HRESULT,
    result_size: usize,
    done: bool,
    cleaned: bool,
    provider_calls: usize,
    cleanup_pending: bool,
    cleanup_called: bool,
    cancel_requested: bool,
    completion_pending: bool,
}

pub struct State {
    provider: usize,
    context: usize,
    identity: usize,
    /// For XAsyncRun.
    work: usize,
    inner: Mutex<Inner>,
    cv: Condvar,
}

/// Live async states indexed by opaque tokens stored in `XAsyncBlock.internal`.
/// Tokens are validated here and never interpreted as pointers, so forged or
/// stale copied blocks fail without touching freed or arbitrary memory.
struct StateEntry {
    block: usize,
    state: Arc<State>,
}

static STATES: LazyLock<Mutex<HashMap<usize, StateEntry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_STATE_TOKEN: AtomicUsize = AtomicUsize::new(0x1_0000);

unsafe fn state_of(b: *mut XAsyncBlock) -> Option<Arc<State>> {
    if b.is_null() {
        return None;
    }
    let p = *((*b).internal.as_ptr() as *const usize);
    if p == 0 {
        return None;
    }
    STATES.lock().unwrap().get(&p).filter(|entry| entry.block == b as usize).map(|entry| entry.state.clone())
}

unsafe fn set_state(b: *mut XAsyncBlock, s: Option<Arc<State>>) {
    let slot = (*b).internal.as_mut_ptr() as *mut usize;
    let old = *slot;
    let mut states = STATES.lock().unwrap();
    let new = if let Some(state) = s {
        loop {
            let token = NEXT_STATE_TOKEN.fetch_add(1, Ordering::Relaxed);
            if token == 0 {
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(entry) = states.entry(token) {
                entry.insert(StateEntry { block: b as usize, state });
                break token;
            }
        }
    } else {
        0
    };
    *slot = new;
    if old != 0 && states.get(&old).is_some_and(|entry| entry.block == b as usize) {
        states.remove(&old);
    }
}

unsafe fn invoke_provider(s: &State, op: u32, b: *mut XAsyncBlock, buffer: *mut c_void, size: usize) -> HRESULT {
    if s.provider == 0 {
        return S_OK;
    }
    let data = XAsyncProviderData { async_: b, buffer_size: size, buffer, context: s.context as *mut c_void };
    let f: XAsyncProvider = std::mem::transmute(s.provider);
    f(op, &data)
}

unsafe fn finish_provider_call(s: &State, b: *mut XAsyncBlock) {
    let cleanup = {
        let mut g = s.inner.lock().unwrap();
        g.provider_calls -= 1;
        if g.provider_calls == 0 && g.cleanup_pending && !g.cleanup_called {
            g.cleanup_called = true;
            true
        } else {
            false
        }
    };
    if cleanup {
        invoke_provider(s, OP_CLEANUP, b, std::ptr::null_mut(), 0);
    }
}

unsafe fn call_provider(s: &State, op: u32, b: *mut XAsyncBlock, buffer: *mut c_void, size: usize) -> HRESULT {
    if s.provider == 0 {
        return S_OK;
    }
    {
        let mut g = s.inner.lock().unwrap();
        // Once completion has requested cleanup, no new provider callback may
        // start with context that is being retired or has already been freed.
        if g.cleanup_pending || g.cleanup_called {
            return E_ABORT;
        }
        g.provider_calls += 1;
    }
    let hr = invoke_provider(s, op, b, buffer, size);
    finish_provider_call(s, b);
    hr
}

unsafe fn request_cleanup(s: &State, b: *mut XAsyncBlock) {
    if s.provider == 0 {
        return;
    }
    let cleanup = {
        let mut g = s.inner.lock().unwrap();
        g.cleanup_pending = true;
        if g.provider_calls == 0 && !g.cleanup_called {
            g.cleanup_called = true;
            true
        } else {
            false
        }
    };
    if cleanup {
        invoke_provider(s, OP_CLEANUP, b, std::ptr::null_mut(), 0);
    }
}

struct WorkContext {
    block: usize,
    state: Arc<State>,
}

unsafe extern "system" fn do_work_cb(ctx: *mut c_void, canceled: bool) {
    let work = Box::from_raw(ctx as *mut WorkContext);
    let b = work.block as *mut XAsyncBlock;
    let s = work.state;
    if s.inner.lock().unwrap().done {
        return;
    }
    if canceled {
        complete_state(b, s, E_ABORT, 0);
        return;
    }
    let hr = if s.work != 0 {
        let f: XAsyncWork = std::mem::transmute(s.work);
        f(b)
    } else {
        call_provider(&s, OP_DO_WORK, b, std::ptr::null_mut(), 0)
    };
    if hr != E_PENDING {
        // providers that finish synchronously in DoWork may not call XAsyncComplete
        let done = s.inner.lock().unwrap().done;
        if !done {
            complete_state(b, s, hr, 0);
        }
    }
}

struct CompletionContext {
    block: usize,
    callback: usize,
    state: Arc<State>,
}

unsafe extern "system" fn completion_cb(ctx: *mut c_void, _canceled: bool) {
    let completion = Box::from_raw(ctx as *mut CompletionContext);
    {
        let mut g = completion.state.inner.lock().unwrap();
        g.completion_pending = false;
    }
    completion.state.cv.notify_all();
    let f: Completion = std::mem::transmute(completion.callback);
    f(completion.block as *mut XAsyncBlock);
}

unsafe fn prepare_block_for_begin(b: *mut XAsyncBlock) -> HRESULT {
    if let Some(old) = state_of(b) {
        let reusable = {
            let g = old.inner.lock().unwrap();
            g.done && g.provider_calls == 0 && !g.completion_pending
        };
        if !reusable {
            return E_INVALIDARG;
        }
        // Reuse is valid once the previous operation has finished. If its
        // result was never consumed, retire that provider context before the
        // block's internal state is replaced.
        request_cleanup(&old, b);
    }
    S_OK
}

pub unsafe extern "system" fn XAsyncBegin(
    b: *mut XAsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    _identity_name: *const u8,
    provider: usize,
) -> HRESULT {
    if let Some((o, f)) = pc(7) {
        let f: unsafe extern "system" fn(usize, *mut XAsyncBlock, *mut c_void, *const c_void, *const u8, usize) -> HRESULT = std::mem::transmute(f);
        return f(o, b, context, identity, _identity_name, provider);
    }
    if b.is_null() || provider == 0 {
        return E_INVALIDARG;
    }
    let prepare = prepare_block_for_begin(b);
    if prepare < 0 {
        return prepare;
    }
    let s = Arc::new(State {
        provider,
        context: context as usize,
        identity: identity as usize,
        work: 0,
        inner: Mutex::new(Inner {
            status: E_PENDING,
            result_size: 0,
            done: false,
            cleaned: false,
            provider_calls: 0,
            cleanup_pending: false,
            cleanup_called: false,
            cancel_requested: false,
            completion_pending: false,
        }),
        cv: Condvar::new(),
    });
    set_state(b, Some(s.clone()));
    let hr = call_provider(&s, OP_BEGIN, b, std::ptr::null_mut(), 0);
    if hr < 0 {
        XAsyncComplete(b, hr, 0);
    }
    hr
}

pub unsafe extern "system" fn XAsyncSchedule(b: *mut XAsyncBlock, delay_ms: u32) -> HRESULT {
    if let Some((o, f)) = pc(9) {
        let f: unsafe extern "system" fn(usize, *mut XAsyncBlock, u32) -> HRESULT = std::mem::transmute(f);
        return f(o, b, delay_ms);
    }
    if b.is_null() {
        return E_INVALIDARG;
    }
    let Some(s) = state_of(b) else { return E_INVALIDARG };
    let Some(q) = taskqueue::queue_from((*b).queue) else {
        return if (*b).queue.is_null() { taskqueue::E_NO_TASK_QUEUE } else { E_INVALIDARG };
    };
    let ctx = Box::into_raw(Box::new(WorkContext { block: b as usize, state: s }));
    taskqueue::submit_delayed(&q, PORT_WORK, delay_ms, Item { cb: do_work_cb as usize, ctx: ctx as usize });
    S_OK
}

unsafe fn complete_state(b: *mut XAsyncBlock, s: Arc<State>, result: HRESULT, required: usize) {
    let completion = if (*b).callback != 0 {
        taskqueue::queue_from((*b).queue).map(|q| (q, (*b).callback))
    } else {
        None
    };
    let cleanup_now;
    {
        let mut g = s.inner.lock().unwrap();
        if g.done {
            return;
        }
        g.status = if result == E_PENDING { crate::util::E_FAIL } else { result };
        g.result_size = if result >= 0 { required } else { 0 };
        g.done = true;
        g.completion_pending = completion.is_some();
        cleanup_now = g.result_size == 0;
        if cleanup_now {
            g.cleaned = true;
        }
    }
    if cleanup_now {
        request_cleanup(&s, b);
    }
    s.cv.notify_all();
    if let Some((q, callback)) = completion {
        let ctx = Box::into_raw(Box::new(CompletionContext { block: b as usize, callback, state: s }));
        taskqueue::submit_now(&q, PORT_COMPLETION, Item { cb: completion_cb as usize, ctx: ctx as usize });
    }
}

pub unsafe extern "system" fn XAsyncComplete(b: *mut XAsyncBlock, result: HRESULT, required: usize) {
    if let Some((o, f)) = pc(10) {
        let f: unsafe extern "system" fn(usize, *mut XAsyncBlock, HRESULT, usize) = std::mem::transmute(f);
        return f(o, b, result, required);
    }
    let Some(s) = state_of(b) else { return };
    complete_state(b, s, result, required);
}

pub unsafe extern "system" fn XAsyncGetStatus(b: *mut XAsyncBlock, wait: bool) -> HRESULT {
    if let Some((o, f)) = pc(3) {
        let f: unsafe extern "system" fn(usize, *mut XAsyncBlock, bool) -> HRESULT = std::mem::transmute(f);
        return f(o, b, wait);
    }
    let Some(s) = state_of(b) else { return E_INVALIDARG };
    let mut g = s.inner.lock().unwrap();
    if wait {
        while !g.done {
            g = s.cv.wait(g).unwrap();
        }
    }
    if g.done {
        g.status
    } else {
        E_PENDING
    }
}

pub unsafe extern "system" fn XAsyncGetResultSize(b: *mut XAsyncBlock, size: *mut usize) -> HRESULT {
    if let Some((o, f)) = pc(4) {
        let f: unsafe extern "system" fn(usize, *mut XAsyncBlock, *mut usize) -> HRESULT = std::mem::transmute(f);
        return f(o, b, size);
    }
    let Some(s) = state_of(b) else { return E_INVALIDARG };
    let g = s.inner.lock().unwrap();
    if !g.done {
        return E_PENDING;
    }
    if !size.is_null() {
        *size = g.result_size;
    }
    g.status
}

pub unsafe extern "system" fn XAsyncGetResult(
    b: *mut XAsyncBlock,
    identity: *const c_void,
    size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    if let Some((o, f)) = pc(11) {
        let f: unsafe extern "system" fn(usize, *mut XAsyncBlock, *const c_void, usize, *mut c_void, *mut usize) -> HRESULT = std::mem::transmute(f);
        return f(o, b, identity, size, buffer, used);
    }
    let Some(s) = state_of(b) else { return E_INVALIDARG };
    if !identity.is_null() && s.identity != 0 && identity as usize != s.identity {
        return E_INVALIDARG;
    }
    let (status, need) = {
        let mut g = s.inner.lock().unwrap();
        if !g.done {
            return E_PENDING;
        }
        if g.status < 0 {
            return g.status;
        }
        if g.result_size > 0 {
            if g.cleaned {
                return E_INVALIDARG; // result already consumed or being consumed
            }
            if size < g.result_size || buffer.is_null() {
                return E_NOT_SUFFICIENT_BUFFER;
            }
            // Claim result delivery while holding the state lock. The provider
            // and cleanup run outside it, but no racing caller can deliver or
            // clean up this result a second time.
            g.cleaned = true;
        }
        (g.status, g.result_size)
    };
    if need > 0 {
        let hr = call_provider(&s, OP_GET_RESULT, b, buffer, size);
        if !used.is_null() {
            *used = need;
        }
        request_cleanup(&s, b);
        return hr;
    }
    if !used.is_null() {
        *used = 0;
    }
    status
}

pub unsafe extern "system" fn XAsyncCancel(b: *mut XAsyncBlock) {
    if let Some((o, f)) = pc(5) {
        let f: unsafe extern "system" fn(usize, *mut XAsyncBlock) = std::mem::transmute(f);
        return f(o, b);
    }
    let Some(s) = state_of(b) else { return };
    {
        let mut g = s.inner.lock().unwrap();
        if g.done || g.cancel_requested {
            return;
        }
        // Only one caller owns provider cancellation. Other racing or replayed
        // requests return without invoking OP_CANCEL or cleanup again.
        g.cancel_requested = true;
    }
    let _ = call_provider(&s, OP_CANCEL, b, std::ptr::null_mut(), 0);
    XAsyncComplete(b, E_ABORT, 0);
}

pub unsafe extern "system" fn XAsyncRun(b: *mut XAsyncBlock, work: usize) -> HRESULT {
    if let Some((o, f)) = pc(6) {
        let f: unsafe extern "system" fn(usize, *mut XAsyncBlock, usize) -> HRESULT = std::mem::transmute(f);
        return f(o, b, work);
    }
    if b.is_null() || work == 0 {
        return E_INVALIDARG;
    }
    let prepare = prepare_block_for_begin(b);
    if prepare < 0 {
        return prepare;
    }
    let s = Arc::new(State {
        provider: 0,
        context: 0,
        identity: 0,
        work,
        inner: Mutex::new(Inner {
            status: E_PENDING,
            result_size: 0,
            done: false,
            cleaned: false,
            provider_calls: 0,
            cleanup_pending: false,
            cleanup_called: false,
            cancel_requested: false,
            completion_pending: false,
        }),
        cv: Condvar::new(),
    });
    set_state(b, Some(s.clone()));
    let hr = XAsyncSchedule(b, 0);
    if hr < 0 {
        complete_state(b, s, hr, 0);
    }
    hr
}

// ---------------------------------------------------------------------------------------
// Helper for our own async APIs: run `work` on the queue, complete with a byte payload.
// ---------------------------------------------------------------------------------------

struct Payload {
    result: Mutex<Option<Result<Vec<u8>, HRESULT>>>,
    work: Mutex<Option<Box<dyn FnOnce() -> Result<Vec<u8>, HRESULT> + Send>>>,
}

unsafe extern "system" fn payload_provider(op: u32, data: *const XAsyncProviderData) -> HRESULT {
    let d = &*data;
    let p = &*(d.context as *const Payload);
    match op {
        OP_BEGIN => XAsyncSchedule(d.async_, 0),
        OP_DO_WORK => {
            let w = p.work.lock().unwrap().take();
            let r = w.map(|f| f()).unwrap_or(Err(crate::util::E_FAIL));
            let (hr, size) = match &r {
                Ok(v) => (S_OK, v.len()),
                Err(e) => (*e, 0),
            };
            *p.result.lock().unwrap() = Some(r);
            XAsyncComplete(d.async_, hr, size);
            S_OK
        }
        OP_GET_RESULT => {
            if let Some(Ok(v)) = p.result.lock().unwrap().as_ref() {
                std::ptr::copy_nonoverlapping(v.as_ptr(), d.buffer as *mut u8, v.len().min(d.buffer_size));
            }
            S_OK
        }
        OP_CLEANUP => {
            drop(Box::from_raw(d.context as *mut Payload));
            S_OK
        }
        _ => S_OK,
    }
}

/// Start an async op whose result is `work()`'s bytes; `identity` ties GetResult calls.
pub unsafe fn begin_with<F>(b: *mut XAsyncBlock, identity: usize, work: F) -> HRESULT
where
    F: FnOnce() -> Result<Vec<u8>, HRESULT> + Send + 'static,
{
    let p = Box::into_raw(Box::new(Payload { result: Mutex::new(None), work: Mutex::new(Some(Box::new(work))) }));
    // On failure XAsyncBegin completes the block, which runs OP_CLEANUP and frees `p`.
    XAsyncBegin(b, p as *mut c_void, identity as *const c_void, std::ptr::null(), payload_provider as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

    static DONE: AtomicU32 = AtomicU32::new(0);
    static OLD_COMPLETION: AtomicU32 = AtomicU32::new(0);
    static NEW_COMPLETION: AtomicU32 = AtomicU32::new(0);
    unsafe extern "system" fn on_done(_b: *mut XAsyncBlock) {
        DONE.fetch_add(1, Ordering::SeqCst);
    }
    unsafe extern "system" fn old_completion(_b: *mut XAsyncBlock) {
        OLD_COMPLETION.fetch_add(1, Ordering::SeqCst);
    }
    unsafe extern "system" fn new_completion(_b: *mut XAsyncBlock) {
        NEW_COMPLETION.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn begin_with_payload_roundtrip() {
    let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            b.callback = on_done as usize;
            let id = 0x1234usize;
            assert_eq!(begin_with(&mut b, id, || Ok(b"hello".to_vec())), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b, true), S_OK);
            let mut size = 0usize;
            assert_eq!(XAsyncGetResultSize(&mut b, &mut size), S_OK);
            assert_eq!(size, 5);
            let mut buf = [0u8; 8];
            let mut used = 0usize;
            assert_eq!(XAsyncGetResult(&mut b, id as _, 2, buf.as_mut_ptr() as _, &mut used), E_NOT_SUFFICIENT_BUFFER);
            assert_eq!(XAsyncGetResult(&mut b, id as _, 8, buf.as_mut_ptr() as _, &mut used), S_OK);
            assert_eq!(&buf[..5], b"hello");
            assert_eq!(used, 5);
            // completion callback is delivered on the (thread pool) completion port
            for _ in 0..200 {
                if DONE.load(Ordering::SeqCst) > 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(DONE.load(Ordering::SeqCst) > 0);
        }
    }

    #[test]
    fn errors_propagate() {
    let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            begin_with(&mut b, 1, || Err(crate::util::E_FAIL));
            assert_eq!(XAsyncGetStatus(&mut b, true), crate::util::E_FAIL);
        }
    }

    struct GatedResult {
        entered: AtomicBool,
        release: AtomicBool,
        get_result_calls: AtomicU32,
        cleanup_calls: AtomicU32,
    }

    unsafe extern "system" fn gated_result_provider(op: u32, data: *const XAsyncProviderData) -> HRESULT {
        let d = &*data;
        let state = &*(d.context as *const GatedResult);
        match op {
            OP_BEGIN => {
                XAsyncComplete(d.async_, S_OK, 4);
                S_OK
            }
            OP_GET_RESULT => {
                state.get_result_calls.fetch_add(1, Ordering::SeqCst);
                state.entered.store(true, Ordering::SeqCst);
                while !state.release.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
                std::ptr::copy_nonoverlapping(b"done".as_ptr(), d.buffer as *mut u8, 4);
                S_OK
            }
            OP_CLEANUP => {
                state.cleanup_calls.fetch_add(1, Ordering::SeqCst);
                S_OK
            }
            _ => S_OK,
        }
    }

    #[test]
    fn concurrent_get_result_claims_provider_once() {
        let _g = ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let state = Box::leak(Box::new(GatedResult {
                entered: AtomicBool::new(false),
                release: AtomicBool::new(false),
                get_result_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let mut block: XAsyncBlock = std::mem::zeroed();
            let identity = 0x5678usize;
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    state as *mut GatedResult as *mut c_void,
                    identity as *const c_void,
                    std::ptr::null(),
                    gated_result_provider as usize,
                ),
                S_OK
            );
            let block_addr = &mut block as *mut XAsyncBlock as usize;
            let first = std::thread::spawn(move || {
                let mut buffer = [0u8; 4];
                let mut used = 0usize;
                let hr = XAsyncGetResult(
                    block_addr as *mut XAsyncBlock,
                    identity as *const c_void,
                    buffer.len(),
                    buffer.as_mut_ptr() as *mut c_void,
                    &mut used,
                );
                (hr, buffer, used)
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !state.entered.load(Ordering::SeqCst) {
                assert!(std::time::Instant::now() < deadline, "first result provider call never started");
                std::thread::yield_now();
            }

            let mut second_buffer = [0u8; 4];
            let second = XAsyncGetResult(
                &mut block,
                identity as *const c_void,
                second_buffer.len(),
                second_buffer.as_mut_ptr() as *mut c_void,
                std::ptr::null_mut(),
            );
            assert_eq!(second, E_INVALIDARG, "racing result call also claimed delivery");
            state.release.store(true, Ordering::SeqCst);
            let (first_hr, first_buffer, first_used) = first.join().unwrap();
            assert_eq!(first_hr, S_OK);
            assert_eq!(&first_buffer, b"done");
            assert_eq!(first_used, 4);
            assert_eq!(state.get_result_calls.load(Ordering::SeqCst), 1);
            assert_eq!(state.cleanup_calls.load(Ordering::SeqCst), 1);
        }
    }

    struct CancelRace {
        entered: AtomicBool,
        release: AtomicBool,
        saw_early_cleanup: AtomicBool,
        do_work_calls: AtomicU32,
        cancel_calls: AtomicU32,
        cleanup_calls: AtomicU32,
    }

    unsafe extern "system" fn cancel_race_provider(op: u32, data: *const XAsyncProviderData) -> HRESULT {
        let d = &*data;
        let state = &*(d.context as *const CancelRace);
        match op {
            OP_BEGIN => XAsyncSchedule(d.async_, 0),
            OP_DO_WORK => {
                state.do_work_calls.fetch_add(1, Ordering::SeqCst);
                state.entered.store(true, Ordering::SeqCst);
                while !state.release.load(Ordering::SeqCst) {
                    if state.cleanup_calls.load(Ordering::SeqCst) != 0 {
                        state.saw_early_cleanup.store(true, Ordering::SeqCst);
                    }
                    std::thread::yield_now();
                }
                if state.cleanup_calls.load(Ordering::SeqCst) != 0 {
                    state.saw_early_cleanup.store(true, Ordering::SeqCst);
                }
                E_PENDING
            }
            OP_CANCEL => {
                state.cancel_calls.fetch_add(1, Ordering::SeqCst);
                S_OK
            }
            OP_CLEANUP => {
                state.cleanup_calls.fetch_add(1, Ordering::SeqCst);
                S_OK
            }
            _ => S_OK,
        }
    }

    #[test]
    fn cancel_defers_cleanup_until_provider_callback_exits() {
        let _g = ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let state = Box::leak(Box::new(CancelRace {
                entered: AtomicBool::new(false),
                release: AtomicBool::new(false),
                saw_early_cleanup: AtomicBool::new(false),
                do_work_calls: AtomicU32::new(0),
                cancel_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let mut queue = std::ptr::null_mut();
            assert_eq!(taskqueue::XTaskQueueCreate(taskqueue::MODE_THREAD_POOL, taskqueue::MODE_MANUAL, &mut queue), S_OK);
            let mut block: XAsyncBlock = std::mem::zeroed();
            block.queue = queue;
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    state as *mut CancelRace as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    cancel_race_provider as usize,
                ),
                S_OK
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !state.entered.load(Ordering::SeqCst) {
                assert!(std::time::Instant::now() < deadline, "provider work never started");
                std::thread::yield_now();
            }

            XAsyncCancel(&mut block);
            assert_eq!(XAsyncGetStatus(&mut block, false), E_ABORT);
            assert_eq!(state.cancel_calls.load(Ordering::SeqCst), 1);
            assert_eq!(state.cleanup_calls.load(Ordering::SeqCst), 0, "cleanup ran inside active provider callback");
            state.release.store(true, Ordering::SeqCst);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while state.cleanup_calls.load(Ordering::SeqCst) == 0 {
                assert!(std::time::Instant::now() < deadline, "deferred cleanup never ran");
                std::thread::yield_now();
            }
            assert!(!state.saw_early_cleanup.load(Ordering::SeqCst));
            assert_eq!(state.do_work_calls.load(Ordering::SeqCst), 1);
            assert_eq!(state.cleanup_calls.load(Ordering::SeqCst), 1);
            assert_eq!(taskqueue::XTaskQueueTerminate(queue, true, std::ptr::null_mut(), 0), S_OK);
            taskqueue::XTaskQueueCloseHandle(queue);
        }
    }

    #[test]
    fn cancel_prevents_queued_provider_entry_after_cleanup() {
        let _g = ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let state = Box::leak(Box::new(CancelRace {
                entered: AtomicBool::new(false),
                release: AtomicBool::new(true),
                saw_early_cleanup: AtomicBool::new(false),
                do_work_calls: AtomicU32::new(0),
                cancel_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let mut queue = std::ptr::null_mut();
            assert_eq!(taskqueue::XTaskQueueCreate(taskqueue::MODE_MANUAL, taskqueue::MODE_MANUAL, &mut queue), S_OK);
            let mut block: XAsyncBlock = std::mem::zeroed();
            block.queue = queue;
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    state as *mut CancelRace as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    cancel_race_provider as usize,
                ),
                S_OK
            );

            XAsyncCancel(&mut block);
            assert_eq!(state.cleanup_calls.load(Ordering::SeqCst), 1);
            assert!(taskqueue::XTaskQueueDispatch(queue, PORT_WORK, 0));
            assert_eq!(state.do_work_calls.load(Ordering::SeqCst), 0, "queued work entered provider after cleanup");
            assert_eq!(state.cleanup_calls.load(Ordering::SeqCst), 1);
            taskqueue::XTaskQueueCloseHandle(queue);
        }
    }

    #[test]
    fn canceled_queued_work_cannot_complete_reused_block() {
        let _g = ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let first = Box::leak(Box::new(CancelRace {
                entered: AtomicBool::new(false),
                release: AtomicBool::new(true),
                saw_early_cleanup: AtomicBool::new(false),
                do_work_calls: AtomicU32::new(0),
                cancel_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let second = Box::leak(Box::new(ReuseProvider {
                complete_on_begin: false,
                begin_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let mut queue = std::ptr::null_mut();
            assert_eq!(taskqueue::XTaskQueueCreate(taskqueue::MODE_MANUAL, taskqueue::MODE_MANUAL, &mut queue), S_OK);
            let mut block: XAsyncBlock = std::mem::zeroed();
            block.queue = queue;
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    first as *mut CancelRace as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    cancel_race_provider as usize,
                ),
                S_OK
            );
            XAsyncCancel(&mut block);
            assert_eq!(XAsyncGetStatus(&mut block, false), E_ABORT);
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    second as *mut ReuseProvider as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    reuse_provider as usize,
                ),
                S_OK
            );
            assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);

            assert!(taskqueue::XTaskQueueDispatch(queue, PORT_WORK, 0));
            assert_eq!(first.do_work_calls.load(Ordering::SeqCst), 0, "canceled generation re-entered its provider");
            assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING, "stale work completed replacement state");
            assert_eq!(second.begin_calls.load(Ordering::SeqCst), 1);
            XAsyncCancel(&mut block);
            taskqueue::XTaskQueueCloseHandle(queue);
        }
    }

    struct CancelOnce {
        entered: AtomicBool,
        release: AtomicBool,
        cancel_calls: AtomicU32,
        cleanup_calls: AtomicU32,
    }

    unsafe extern "system" fn cancel_once_provider(op: u32, data: *const XAsyncProviderData) -> HRESULT {
        let d = &*data;
        let state = &*(d.context as *const CancelOnce);
        match op {
            OP_BEGIN => S_OK,
            OP_CANCEL => {
                state.cancel_calls.fetch_add(1, Ordering::SeqCst);
                state.entered.store(true, Ordering::SeqCst);
                while !state.release.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
                S_OK
            }
            OP_CLEANUP => {
                state.cleanup_calls.fetch_add(1, Ordering::SeqCst);
                S_OK
            }
            _ => S_OK,
        }
    }

    #[test]
    fn concurrent_cancel_invokes_provider_once() {
        let _g = ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let state = Box::leak(Box::new(CancelOnce {
                entered: AtomicBool::new(false),
                release: AtomicBool::new(false),
                cancel_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let mut block: XAsyncBlock = std::mem::zeroed();
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    state as *mut CancelOnce as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    cancel_once_provider as usize,
                ),
                S_OK
            );
            let block_addr = &mut block as *mut XAsyncBlock as usize;
            let first = std::thread::spawn(move || XAsyncCancel(block_addr as *mut XAsyncBlock));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !state.entered.load(Ordering::SeqCst) {
                assert!(std::time::Instant::now() < deadline, "first cancellation never entered provider");
                std::thread::yield_now();
            }

            XAsyncCancel(&mut block);
            XAsyncCancel(&mut block);
            assert_eq!(state.cancel_calls.load(Ordering::SeqCst), 1, "racing cancellation re-entered provider");
            assert_eq!(state.cleanup_calls.load(Ordering::SeqCst), 0);
            state.release.store(true, Ordering::SeqCst);
            first.join().unwrap();
            assert_eq!(XAsyncGetStatus(&mut block, false), E_ABORT);
            assert_eq!(state.cancel_calls.load(Ordering::SeqCst), 1);
            assert_eq!(state.cleanup_calls.load(Ordering::SeqCst), 1);
            XAsyncCancel(&mut block);
            assert_eq!(state.cancel_calls.load(Ordering::SeqCst), 1, "post-completion cancellation re-entered provider");
            assert_eq!(state.cleanup_calls.load(Ordering::SeqCst), 1);
        }
    }

    struct EmptyResult {
        get_result_calls: AtomicU32,
        cleanup_calls: AtomicU32,
    }

    unsafe extern "system" fn empty_result_provider(op: u32, data: *const XAsyncProviderData) -> HRESULT {
        let d = &*data;
        let state = &*(d.context as *const EmptyResult);
        match op {
            OP_BEGIN => {
                XAsyncComplete(d.async_, 7, 0);
                S_OK
            }
            OP_GET_RESULT => {
                state.get_result_calls.fetch_add(1, Ordering::SeqCst);
                S_OK
            }
            OP_CLEANUP => {
                state.cleanup_calls.fetch_add(1, Ordering::SeqCst);
                S_OK
            }
            _ => S_OK,
        }
    }

    #[test]
    fn empty_result_preserves_status_and_cleans_once() {
        let _g = ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let state = Box::leak(Box::new(EmptyResult {
                get_result_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let mut block: XAsyncBlock = std::mem::zeroed();
            let identity = 0x9abcusize;
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    state as *mut EmptyResult as *mut c_void,
                    identity as *const c_void,
                    std::ptr::null(),
                    empty_result_provider as usize,
                ),
                S_OK
            );
            assert_eq!(XAsyncGetStatus(&mut block, false), 7);
            let mut size = usize::MAX;
            assert_eq!(XAsyncGetResultSize(&mut block, &mut size), 7);
            assert_eq!(size, 0);
            assert_eq!(state.cleanup_calls.load(Ordering::SeqCst), 1);

            for _ in 0..3 {
                let mut used = usize::MAX;
                assert_eq!(
                    XAsyncGetResult(
                        &mut block,
                        identity as *const c_void,
                        0,
                        std::ptr::null_mut(),
                        &mut used,
                    ),
                    7
                );
                assert_eq!(used, 0);
            }
            assert_eq!(state.get_result_calls.load(Ordering::SeqCst), 0, "empty result entered OP_GET_RESULT");
            assert_eq!(state.cleanup_calls.load(Ordering::SeqCst), 1, "empty result cleaned more than once");
        }
    }

    struct ReuseProvider {
        complete_on_begin: bool,
        begin_calls: AtomicU32,
        cleanup_calls: AtomicU32,
    }

    unsafe extern "system" fn reuse_provider(op: u32, data: *const XAsyncProviderData) -> HRESULT {
        let d = &*data;
        let state = &*(d.context as *const ReuseProvider);
        match op {
            OP_BEGIN => {
                state.begin_calls.fetch_add(1, Ordering::SeqCst);
                if state.complete_on_begin {
                    XAsyncComplete(d.async_, S_OK, 0);
                }
                S_OK
            }
            OP_CLEANUP => {
                state.cleanup_calls.fetch_add(1, Ordering::SeqCst);
                S_OK
            }
            _ => S_OK,
        }
    }

    #[test]
    fn double_begin_rejects_active_state_and_completed_reuse_cleans_old_result() {
        let _g = ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let first = Box::leak(Box::new(ReuseProvider {
                complete_on_begin: false,
                begin_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let second = Box::leak(Box::new(ReuseProvider {
                complete_on_begin: true,
                begin_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let mut block: XAsyncBlock = std::mem::zeroed();
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    first as *mut ReuseProvider as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    reuse_provider as usize,
                ),
                S_OK
            );
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    second as *mut ReuseProvider as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    reuse_provider as usize,
                ),
                E_INVALIDARG,
                "active block was shared with a second operation"
            );
            assert_eq!(first.begin_calls.load(Ordering::SeqCst), 1);
            assert_eq!(second.begin_calls.load(Ordering::SeqCst), 0);
            assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);
            assert_eq!(XAsyncRun(&mut block, run_work as usize), E_INVALIDARG, "XAsyncRun overwrote an active operation");
            assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);

            // Leave a non-empty result unconsumed. Reusing the now-completed
            // block must clean its old provider exactly once before replacement.
            XAsyncComplete(&mut block, S_OK, 4);
            assert_eq!(first.cleanup_calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    second as *mut ReuseProvider as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    reuse_provider as usize,
                ),
                S_OK
            );
            assert_eq!(first.cleanup_calls.load(Ordering::SeqCst), 1);
            assert_eq!(second.begin_calls.load(Ordering::SeqCst), 1);
            assert_eq!(second.cleanup_calls.load(Ordering::SeqCst), 1);
            assert_eq!(XAsyncGetStatus(&mut block, false), S_OK);
        }
    }

    #[test]
    fn reuse_waits_for_original_completion_callback() {
        let _g = ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            OLD_COMPLETION.store(0, Ordering::SeqCst);
            NEW_COMPLETION.store(0, Ordering::SeqCst);
            let first = Box::leak(Box::new(ReuseProvider {
                complete_on_begin: true,
                begin_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let second = Box::leak(Box::new(ReuseProvider {
                complete_on_begin: true,
                begin_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let mut queue = std::ptr::null_mut();
            assert_eq!(taskqueue::XTaskQueueCreate(taskqueue::MODE_MANUAL, taskqueue::MODE_MANUAL, &mut queue), S_OK);
            let mut block: XAsyncBlock = std::mem::zeroed();
            block.queue = queue;
            block.callback = old_completion as usize;
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    first as *mut ReuseProvider as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    reuse_provider as usize,
                ),
                S_OK
            );
            assert_eq!(XAsyncGetStatus(&mut block, false), S_OK);
            assert_eq!(OLD_COMPLETION.load(Ordering::SeqCst), 0);
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    second as *mut ReuseProvider as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    reuse_provider as usize,
                ),
                E_INVALIDARG,
                "block was reused before its original completion callback"
            );
            assert_eq!(second.begin_calls.load(Ordering::SeqCst), 0);

            assert!(taskqueue::XTaskQueueDispatch(queue, PORT_COMPLETION, 0));
            assert_eq!(OLD_COMPLETION.load(Ordering::SeqCst), 1);
            block.callback = new_completion as usize;
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    second as *mut ReuseProvider as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    reuse_provider as usize,
                ),
                S_OK
            );
            assert_eq!(second.begin_calls.load(Ordering::SeqCst), 1);
            assert!(taskqueue::XTaskQueueDispatch(queue, PORT_COMPLETION, 0));
            assert_eq!(OLD_COMPLETION.load(Ordering::SeqCst), 1, "stale completion invoked old callback again");
            assert_eq!(NEW_COMPLETION.load(Ordering::SeqCst), 1);
            taskqueue::XTaskQueueCloseHandle(queue);
        }
    }

    struct ReentrantReuse {
        provider_context: AtomicUsize,
        begin_result: AtomicU32,
    }

    unsafe extern "system" fn reuse_from_completion(block: *mut XAsyncBlock) {
        let reuse = &*((*block).context as *const ReentrantReuse);
        (*block).callback = 0;
        let hr = XAsyncBegin(
            block,
            reuse.provider_context.load(Ordering::SeqCst) as *mut c_void,
            std::ptr::null(),
            std::ptr::null(),
            reuse_provider as usize,
        );
        reuse.begin_result.store(hr as u32, Ordering::SeqCst);
    }

    #[test]
    fn completion_callback_can_reuse_its_block() {
        let _g = ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let first = Box::leak(Box::new(ReuseProvider {
                complete_on_begin: true,
                begin_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let second = Box::leak(Box::new(ReuseProvider {
                complete_on_begin: true,
                begin_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let reuse = Box::leak(Box::new(ReentrantReuse {
                provider_context: AtomicUsize::new(second as *mut ReuseProvider as usize),
                begin_result: AtomicU32::new(E_PENDING as u32),
            }));
            let mut queue = std::ptr::null_mut();
            assert_eq!(taskqueue::XTaskQueueCreate(taskqueue::MODE_MANUAL, taskqueue::MODE_MANUAL, &mut queue), S_OK);
            let mut block: XAsyncBlock = std::mem::zeroed();
            block.queue = queue;
            block.context = reuse as *mut ReentrantReuse as *mut c_void;
            block.callback = reuse_from_completion as usize;
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    first as *mut ReuseProvider as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    reuse_provider as usize,
                ),
                S_OK
            );
            assert!(taskqueue::XTaskQueueDispatch(queue, PORT_COMPLETION, 0));
            assert_eq!(reuse.begin_result.load(Ordering::SeqCst) as HRESULT, S_OK);
            assert_eq!(second.begin_calls.load(Ordering::SeqCst), 1);
            assert_eq!(XAsyncGetStatus(&mut block, false), S_OK);
            assert!(!taskqueue::XTaskQueueDispatch(queue, PORT_COMPLETION, 0));
            taskqueue::XTaskQueueCloseHandle(queue);
        }
    }

    #[test]
    fn forged_and_stale_state_tokens_fail_without_dereference() {
        let _g = ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let mut forged: XAsyncBlock = std::mem::zeroed();
            *(forged.internal.as_mut_ptr() as *mut usize) = usize::MAX;
            assert_eq!(XAsyncGetStatus(&mut forged, false), E_INVALIDARG);
            assert_eq!(XAsyncGetResultSize(&mut forged, std::ptr::null_mut()), E_INVALIDARG);
            XAsyncCancel(&mut forged);
            XAsyncComplete(&mut forged, S_OK, 0);

            let first = Box::leak(Box::new(ReuseProvider {
                complete_on_begin: true,
                begin_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let second = Box::leak(Box::new(ReuseProvider {
                complete_on_begin: true,
                begin_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let copied = Box::leak(Box::new(ReuseProvider {
                complete_on_begin: true,
                begin_calls: AtomicU32::new(0),
                cleanup_calls: AtomicU32::new(0),
            }));
            let mut block: XAsyncBlock = std::mem::zeroed();
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    first as *mut ReuseProvider as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    reuse_provider as usize,
                ),
                S_OK
            );
            let mut stale = std::ptr::read(&block);
            assert_eq!(XAsyncGetStatus(&mut stale, false), E_INVALIDARG, "live token was accepted from a copied block");
            let mut copied_block = std::ptr::read(&block);
            assert_eq!(
                XAsyncBegin(
                    &mut copied_block,
                    copied as *mut ReuseProvider as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    reuse_provider as usize,
                ),
                S_OK
            );
            assert_eq!(XAsyncGetStatus(&mut block, false), S_OK, "beginning a copied block retired its owner's state");
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    second as *mut ReuseProvider as *mut c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                    reuse_provider as usize,
                ),
                S_OK
            );
            assert_eq!(XAsyncGetStatus(&mut stale, false), E_INVALIDARG);
            XAsyncCancel(&mut stale);
            XAsyncComplete(&mut stale, E_ABORT, 0);
            assert_eq!(XAsyncGetStatus(&mut block, false), S_OK, "stale copied block changed live replacement state");
            assert_eq!(first.cleanup_calls.load(Ordering::SeqCst), 1);
            assert_eq!(second.cleanup_calls.load(Ordering::SeqCst), 1);
            assert_eq!(copied.cleanup_calls.load(Ordering::SeqCst), 1);
        }
    }

    unsafe extern "system" fn run_work(_b: *mut XAsyncBlock) -> HRESULT {
        7
    }

    #[test]
    fn xasync_run() {
    let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XAsyncRun(&mut b, run_work as usize), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b, true), 7);
        }
    }
}
