//! XTaskQueue – GDK's async dispatch primitive (contract: libHttpClient XTaskQueue.h, MIT).
//!
//! Dispatch modes: Manual (caller pumps `XTaskQueueDispatch`), ThreadPool,
//! SerializedThreadPool (pool, one at a time) and Immediate (run on the submitting
//! thread). Callbacks submitted after `XTaskQueueTerminate` run with `canceled = true`.

#![allow(non_snake_case)]

use std::cell::RefCell;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::util::{E_INVALIDARG, HRESULT, S_OK};

pub const MODE_MANUAL: u32 = 0;
pub const MODE_THREAD_POOL: u32 = 1;
pub const MODE_SERIALIZED_THREAD_POOL: u32 = 2;
pub const MODE_IMMEDIATE: u32 = 3;
pub const PORT_WORK: u32 = 0;
pub const PORT_COMPLETION: u32 = 1;
pub const E_ABORT_QUEUE_TERMINATED: HRESULT = 0x8004_0001u32 as i32; // E_ABORT-like
pub const E_NO_TASK_QUEUE: HRESULT = 0x8007_01ABu32 as i32;

pub type XTaskQueueCallback = unsafe extern "system" fn(*mut c_void, bool);
pub type XTaskQueueTerminatedCallback = unsafe extern "system" fn(*mut c_void);
/// Monitor notification (contract: libHttpClient `XTaskQueueMonitorCallback`):
/// `void(context, queue, port)`, invoked synchronously on the submitting
/// thread each time a callback is submitted to the queue for execution.
/// `queue` is a stable logical handle owned by the monitor registration, not
/// the registration handle the game registered through.
pub type XTaskQueueMonitorCallback = unsafe extern "system" fn(*mut c_void, *mut c_void, u32);

#[derive(Clone, Copy)]
pub struct Item {
    pub cb: usize,
    pub ctx: usize,
}

impl Item {
    pub unsafe fn run(self, canceled: bool) {
        let f: XTaskQueueCallback = std::mem::transmute(self.cb);
        f(self.ctx as *mut c_void, canceled);
    }
}

pub struct Port {
    mode: u32,
    items: Mutex<VecDeque<Item>>,
    cv: Condvar,
    running: AtomicBool,
}

impl Port {
    fn new(mode: u32) -> Port {
        Port { mode, items: Mutex::new(VecDeque::new()), cv: Condvar::new(), running: AtomicBool::new(false) }
    }
}

pub struct Queue {
    ports: [Port; 2],
    terminated: AtomicBool,
    inflight: AtomicUsize,
    idle: (Mutex<()>, Condvar),
}

/// Opaque type used only in the public handle signature. Handle values are
/// monotonic tokens, not pointers to this type, and are never dereferenced.
pub struct QueueHandle {
    _opaque: [u8; 0],
}

// ---------------------------------------------------------------------------------------
// shared worker pool + timer
// ---------------------------------------------------------------------------------------

type Job = Box<dyn FnOnce() + Send>;

fn pool() -> &'static Mutex<mpsc::Sender<Job>> {
    static POOL: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
        for i in 0..n {
            let rx = rx.clone();
            std::thread::Builder::new()
                .name(format!("xcl-taskqueue-{i}"))
                .spawn(move || loop {
                    let job = { rx.lock().unwrap().recv() };
                    match job {
                        Ok(j) => j(),
                        Err(_) => break,
                    }
                })
                .expect("spawn task queue worker");
        }
        Mutex::new(tx)
    })
}

fn spawn(job: Job) {
    let _ = pool().lock().unwrap().send(job);
}

struct Timed {
    at: Instant,
    seq: u64,
    queue: Arc<Queue>,
    port: u32,
    item: Item,
}
impl PartialEq for Timed {
    fn eq(&self, o: &Self) -> bool {
        self.at == o.at && self.seq == o.seq
    }
}
impl Eq for Timed {}
impl PartialOrd for Timed {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Timed {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        // min-heap on (at, seq)
        o.at.cmp(&self.at).then(o.seq.cmp(&self.seq))
    }
}

fn timer() -> &'static (Mutex<BinaryHeap<Timed>>, Condvar) {
    static T: OnceLock<(Mutex<BinaryHeap<Timed>>, Condvar)> = OnceLock::new();
    static STARTED: AtomicBool = AtomicBool::new(false);
    let t = T.get_or_init(|| (Mutex::new(BinaryHeap::new()), Condvar::new()));
    if !STARTED.swap(true, Ordering::AcqRel) {
        std::thread::Builder::new()
            .name("xcl-taskqueue-timer".into())
            .spawn(|| {
                let (m, cv) = timer();
                let mut g = m.lock().unwrap();
                loop {
                    let now = Instant::now();
                    match g.peek() {
                        None => g = cv.wait(g).unwrap(),
                        Some(t) if t.at <= now => {
                            let t = g.pop().unwrap();
                            drop(g);
                            // the entry was counted when it was scheduled;
                            // enqueue must not bill it a second time
                            enqueue(&t.queue, t.port, t.item);
                            g = m.lock().unwrap();
                        }
                        Some(t) => {
                            let d = t.at - now;
                            g = cv.wait_timeout(g, d).unwrap().0;
                        }
                    }
                }
            })
            .expect("spawn timer");
    }
    t
}

static SEQ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------------------
// core operations
// ---------------------------------------------------------------------------------------

impl Queue {
    pub fn new(work_mode: u32, completion_mode: u32) -> Arc<Queue> {
        Arc::new(Queue {
            ports: [Port::new(work_mode), Port::new(completion_mode)],
            terminated: AtomicBool::new(false),
            inflight: AtomicUsize::new(0),
            idle: (Mutex::new(()), Condvar::new()),
        })
    }

    fn done_one(&self) {
        if self.inflight.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _g = self.idle.0.lock().unwrap();
            self.idle.1.notify_all();
        }
    }

    unsafe fn execute(self: &Arc<Queue>, item: Item) {
        let canceled = self.terminated.load(Ordering::Acquire);
        item.run(canceled);
        self.done_one();
    }
}

fn drain_serialized(q: Arc<Queue>, port: u32) {
    let p = &q.ports[port as usize];
    loop {
        let next = p.items.lock().unwrap().pop_front();
        match next {
            Some(it) => unsafe { q.execute(it) },
            None => {
                p.running.store(false, Ordering::Release);
                // re-check: an item may have arrived after pop but before the store
                let more = !p.items.lock().unwrap().is_empty();
                if more && !p.running.swap(true, Ordering::AcqRel) {
                    continue;
                }
                break;
            }
        }
    }
}

/// Route an already-counted item onto its port. Never bills `inflight` again:
/// callers either incremented it themselves (submit_now) or it was billed at
/// schedule time (pending timers carry their count until they run or are
/// canceled by `XTaskQueueTerminate`).
fn enqueue(q: &Arc<Queue>, port: u32, item: Item) {
    let p = &q.ports[port as usize];
    match p.mode {
        MODE_IMMEDIATE => unsafe { q.execute(item) },
        MODE_THREAD_POOL => {
            let q2 = q.clone();
            spawn(Box::new(move || unsafe { q2.execute(item) }));
        }
        MODE_SERIALIZED_THREAD_POOL => {
            // the terminated check and the push are under the same lock as
            // terminate's drain: a fired timer racing termination either lands
            // in the deque before draining or runs canceled now – never parked
            let mut g = p.items.lock().unwrap();
            if q.terminated.load(Ordering::Acquire) {
                drop(g);
                unsafe { q.execute(item) };
                return;
            }
            g.push_back(item);
            drop(g);
            if !p.running.swap(true, Ordering::AcqRel) {
                let q2 = q.clone();
                spawn(Box::new(move || drain_serialized(q2, port)));
            }
        }
        _ => {
            let mut g = p.items.lock().unwrap();
            if q.terminated.load(Ordering::Acquire) {
                drop(g);
                unsafe { q.execute(item) };
                return;
            }
            g.push_back(item);
            drop(g);
            p.cv.notify_one();
        }
    }
}

pub fn submit_now(q: &Arc<Queue>, port: u32, item: Item) {
    q.inflight.fetch_add(1, Ordering::AcqRel);
    // Notify at submission, before dispatch. In particular, an Immediate port
    // must not run the queued callback before observers learn it was submitted.
    notify_monitors(q, port);
    enqueue(q, port, item);
}

pub fn submit_delayed(q: &Arc<Queue>, port: u32, delay_ms: u32, item: Item) {
    if delay_ms == 0 {
        return submit_now(q, port, item);
    }
    let (m, cv) = timer();
    // in-flight from the moment it is scheduled, so terminate(wait) and the
    // termination callback can never slip past a pending timer
    q.inflight.fetch_add(1, Ordering::AcqRel);
    let mut g = m.lock().unwrap();
    if q.terminated.load(Ordering::Acquire) {
        // submitted after termination: run it canceled right now, exactly once
        drop(g);
        notify_monitors(q, port);
        unsafe { q.execute(item) };
        return;
    }
    g.push(Timed {
        at: Instant::now() + Duration::from_millis(delay_ms as u64),
        seq: SEQ.fetch_add(1, Ordering::Relaxed),
        queue: q.clone(),
        port,
        item,
    });
    drop(g);
    cv.notify_all();
    // monitors learn of the submission itself, exactly once and now – the
    // timer promoting the item onto the port at its deadline is dispatch,
    // not a new submission, and must never re-notify
    notify_monitors(q, port);
}

/// Dispatch one item from a Manual port. Returns true when something ran.
pub fn dispatch(q: &Arc<Queue>, port: u32, timeout_ms: u32) -> bool {
    let p = &q.ports[port as usize];
    let mut g = p.items.lock().unwrap();
    if g.is_empty() && timeout_ms != 0 {
        let dur = if timeout_ms == u32::MAX { Duration::from_secs(u64::MAX / 4) } else { Duration::from_millis(timeout_ms as u64) };
        let deadline = Instant::now() + dur.min(Duration::from_secs(365 * 24 * 3600));
        while g.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            g = p.cv.wait_timeout(g, deadline - now).unwrap().0;
        }
    }
    let Some(item) = g.pop_front() else { return false };
    drop(g);
    unsafe { q.execute(item) };
    true
}

// ---------------------------------------------------------------------------------------
// handles + process queue
// ---------------------------------------------------------------------------------------

/// Live registry of every outstanding `XTaskQueueHandle`, keyed by opaque token.
/// Handle liveness is proven here rather than by dereferencing the pointer, so
/// forged, stale, interior or already-closed addresses reject cleanly, and the
/// atomic check-and-remove in `XTaskQueueCloseHandle` makes close idempotent.
/// The lock is taken only for the map operation itself – never held
/// across callbacks or queue work.
static LIVE: LazyLock<Mutex<HashMap<usize, Arc<Queue>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_HANDLE: AtomicUsize = AtomicUsize::new(0x1_0000);

pub fn new_handle(q: Arc<Queue>) -> *mut QueueHandle {
    // Tokens are never dereferenced or deliberately reused. Stepping by 16
    // keeps them pointer-aligned for callers that treat handles as opaque
    // pointer-sized values. The occupied check also makes wraparound safe.
    loop {
        let token = NEXT_HANDLE.fetch_add(16, Ordering::Relaxed);
        if token == 0 {
            continue;
        }
        let mut live = LIVE.lock().unwrap();
        if let std::collections::hash_map::Entry::Vacant(e) = live.entry(token) {
            e.insert(q);
            return token as *mut QueueHandle;
        }
    }
}

pub unsafe fn queue_from(h: *mut c_void) -> Option<Arc<Queue>> {
    if h.is_null() {
        return process_queue();
    }
    LIVE.lock().unwrap().get(&(h as usize)).cloned()
}

struct ProcessQueueState {
    initialized: bool,
    queue: Option<Arc<Queue>>,
}

impl ProcessQueueState {
    fn current(&mut self) -> Option<Arc<Queue>> {
        if !self.initialized {
            self.initialized = true;
            self.queue = Some(Queue::new(MODE_THREAD_POOL, MODE_THREAD_POOL));
        }
        self.queue.clone()
    }

    fn set(&mut self, queue: Option<Arc<Queue>>) {
        self.initialized = true;
        self.queue = queue;
    }
}

static PROCESS_QUEUE: Mutex<ProcessQueueState> = Mutex::new(ProcessQueueState {
    initialized: false,
    queue: None,
});

pub fn process_queue() -> Option<Arc<Queue>> {
    PROCESS_QUEUE.lock().unwrap().current()
}

// ---------------------------------------------------------------------------------------
// exported API
// ---------------------------------------------------------------------------------------

pub unsafe extern "system" fn XTaskQueueCreate(work: u32, completion: u32, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() || work > 3 || completion > 3 {
        return E_INVALIDARG;
    }
    *out = new_handle(Queue::new(work, completion)) as *mut c_void;
    S_OK
}

pub unsafe extern "system" fn XTaskQueueDuplicateHandle(h: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    match queue_from(h) {
        Some(q) => {
            *out = new_handle(q) as *mut c_void;
            S_OK
        }
        None => E_INVALIDARG,
    }
}

pub unsafe extern "system" fn XTaskQueueCloseHandle(h: *mut c_void) {
    if h.is_null() {
        return;
    }
    // The remove is one locked claim. Foreign, stale, interior and repeat
    // closes observe `None`; no caller-provided address is ever dereferenced.
    LIVE.lock().unwrap().remove(&(h as usize));
}

pub unsafe extern "system" fn XTaskQueueSubmitCallback(h: *mut c_void, port: u32, ctx: *mut c_void, cb: usize) -> HRESULT {
    if cb == 0 || port > 1 {
        return E_INVALIDARG;
    }
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    submit_now(&q, port, Item { cb, ctx: ctx as usize });
    S_OK
}

pub unsafe extern "system" fn XTaskQueueSubmitDelayedCallback(h: *mut c_void, port: u32, delay: u32, ctx: *mut c_void, cb: usize) -> HRESULT {
    if cb == 0 || port > 1 {
        return E_INVALIDARG;
    }
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    submit_delayed(&q, port, delay, Item { cb, ctx: ctx as usize });
    S_OK
}

pub unsafe extern "system" fn XTaskQueueDispatch(h: *mut c_void, port: u32, timeout: u32) -> bool {
    if port > 1 {
        return false;
    }
    match queue_from(h) {
        Some(q) => dispatch(&q, port, timeout),
        None => false,
    }
}

pub unsafe extern "system" fn XTaskQueueIsEmpty(h: *mut c_void, port: u32) -> bool {
    match queue_from(h) {
        Some(q) if port <= 1 => q.ports[port as usize].items.lock().unwrap().is_empty() && q.inflight.load(Ordering::Acquire) == 0,
        _ => true,
    }
}

pub unsafe extern "system" fn XTaskQueueTerminate(h: *mut c_void, wait: bool, ctx: *mut c_void, cb: usize) -> HRESULT {
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    q.terminated.store(true, Ordering::Release);
    // pull this queue's pending timers out of the shared heap. Each was billed
    // in-flight when scheduled, so each gets exactly one canceled run below;
    // entries that the timer thread already popped take the normal dispatch
    // path and still land with canceled = true
    let due: Vec<Item> = {
        let (m, _cv) = timer();
        let mut g = m.lock().unwrap();
        let mut due = Vec::new();
        for t in std::mem::take(&mut *g) {
            if Arc::ptr_eq(&t.queue, &q) {
                due.push(t.item);
            } else {
                g.push(t);
            }
        }
        due
    };
    // cancel everything still queued on manual/serialized ports
    for port in 0..2 {
        let pending: Vec<Item> = q.ports[port].items.lock().unwrap().drain(..).collect();
        for it in pending {
            q.execute(it);
        }
    }
    for it in due {
        q.execute(it);
    }
    let ctx = ctx as usize; // raw pointers are not Send
    let finish = move || {
        let (m, cv) = &q.idle;
        let mut g = m.lock().unwrap();
        while q.inflight.load(Ordering::Acquire) != 0 {
            g = cv.wait_timeout(g, Duration::from_millis(50)).unwrap().0;
        }
        drop(g);
        if cb != 0 {
            let f: XTaskQueueTerminatedCallback = std::mem::transmute(cb);
            f(ctx as *mut c_void);
        }
    };
    if wait {
        finish();
    } else {
        spawn(Box::new(finish));
    }
    S_OK
}

pub unsafe extern "system" fn XTaskQueueGetCurrentProcessTaskQueue(out: *mut *mut c_void) -> bool {
    if out.is_null() {
        return false;
    }
    match process_queue() {
        Some(queue) => {
            *out = new_handle(queue) as *mut c_void;
            true
        }
        None => {
            *out = std::ptr::null_mut();
            false
        }
    }
}

pub unsafe extern "system" fn XTaskQueueSetCurrentProcessTaskQueue(h: *mut c_void) {
    if h.is_null() {
        PROCESS_QUEUE.lock().unwrap().set(None);
    } else if let Some(queue) = queue_from(h) {
        PROCESS_QUEUE.lock().unwrap().set(Some(queue));
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RegistrationToken {
    pub token: u64,
}

/// Context block handed privately to the Windows wait callback
/// (`waiter_fired`). The game only ever sees the opaque u64 token; this block
/// is owned by `WAITERS` and freed by the unregister claim after
/// `UnregisterWaitEx` has drained every callback already in flight.
struct Waiter {
    queue: Arc<Queue>,
    port: u32,
    item: Item,
}

/// One armed waiter as `WAITERS` knows it: the OS wait handle to tear down,
/// the raw context block it was registered with, and the queue it belongs to
/// (proves the queue/token association on unregister, via `Arc::ptr_eq`, so a
/// token armed on queue A cannot be claimed through queue B's handle).
struct WaiterReg {
    queue: Arc<Queue>,
    wait: usize,
    waiter: usize,
}

/// Live waiter registry, parallel in spirit to `LIVE` for queue handles:
/// opaque monotonic tokens → the registration record. The check-and-remove in
/// `XTaskQueueUnregisterWaiter` is a single locked claim, so forged, stale,
/// double-used and wrong-queue tokens observe `None` and a replayed claim can
/// never dereference or double-free the context block. The lock spans map
/// operations only – `UnregisterWaitEx` runs outside it because it waits for
/// in-flight callbacks that may call back into this module.
static WAITERS: LazyLock<Mutex<HashMap<u64, WaiterReg>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// Waiter and monitor tokens are minted from ONE monotonic sequence seeded at
/// 2 (0 reads as unset, 1 stays reserved). A shared counter makes the domains
/// mutually exclusive by construction: a monitor token can never equal a live
/// waiter token and vice versa, so a wrong-domain unregister always misses.
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(2);

unsafe extern "system" fn waiter_fired(ctx: *mut c_void, _timed_out: bool) {
    let w = &*(ctx as *const Waiter);
    submit_now(&w.queue, w.port, w.item);
}

pub unsafe extern "system" fn XTaskQueueRegisterWaiter(
    h: *mut c_void,
    port: u32,
    handle: *mut c_void,
    ctx: *mut c_void,
    cb: usize,
    token: *mut RegistrationToken,
) -> HRESULT {
    use windows_sys::Win32::Foundation::{GetHandleInformation, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Threading::{RegisterWaitForSingleObject, INFINITE, WT_EXECUTEDEFAULT};
    // every argument is validated before anything is armed: a bad `port` must
    // never reach `enqueue`'s queue-ports index, and the OS object must be a
    // real kernel handle in this process – never null, pseudo or forged
    if token.is_null() || cb == 0 || port > 1 || handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return E_INVALIDARG;
    }
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    let mut flags = 0u32;
    if GetHandleInformation(handle, &mut flags) == 0 {
        return E_INVALIDARG;
    }
    let w = Box::into_raw(Box::new(Waiter { queue: q.clone(), port, item: Item { cb, ctx: ctx as usize } }));
    let mut wh = std::ptr::null_mut();
    if RegisterWaitForSingleObject(&mut wh, handle, Some(waiter_fired), w as *const c_void, INFINITE, WT_EXECUTEDEFAULT) == 0 {
        drop(Box::from_raw(w));
        return crate::util::E_FAIL;
    }
    // armed and recorded in one locked insertion; wraparound-safe like handles
    let mut g = WAITERS.lock().unwrap();
    loop {
        let id = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        if id <= 1 {
            continue;
        }
        if let std::collections::hash_map::Entry::Vacant(e) = g.entry(id) {
            e.insert(WaiterReg { queue: q, wait: wh as usize, waiter: w as usize });
            (*token).token = id;
            return S_OK;
        }
    }
}

pub unsafe extern "system" fn XTaskQueueUnregisterWaiter(h: *mut c_void, token: RegistrationToken) {
    use windows_sys::Win32::System::Threading::UnregisterWaitEx;
    let Some(q) = queue_from(h) else { return };
    // a single locked claim: the token must be live AND armed for this queue.
    // Forged, stale, double-used and wrong-queue tokens miss without anything
    // being dereferenced or freed
    let reg = {
        let mut g = WAITERS.lock().unwrap();
        match g.entry(token.token) {
            std::collections::hash_map::Entry::Occupied(e) if Arc::ptr_eq(&e.get().queue, &q) => e.remove(),
            _ => return,
        }
    };
    // outside the registry lock: INVALID_HANDLE_VALUE makes UnregisterWaitEx
    // wait for callbacks already in flight, so the claim below frees the
    // context block without racing a callback – and in-flight callbacks (or a
    // reentrant unregister) cannot deadlock on WAITERS
    UnregisterWaitEx(reg.wait as *mut c_void, windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE);
    drop(Box::from_raw(reg.waiter as *mut Waiter));
}

/// Armed/observing state of one monitor: `armed` closes exactly once under
/// the same lock that bills in-flight callbacks. A notifier therefore either
/// bills before the unregister claim (and the claim waits for that callback
/// to finish) or observes `armed == false` and never calls – so no monitor
/// callback can start after `XTaskQueueUnregisterMonitor` returns.
struct MonitorState {
    armed: bool,
    inflight: usize,
}

/// One armed monitor as `MONITORS` knows it: the queue it watches (proves the
/// queue/token association on unregister via `Arc::ptr_eq`), the callback and
/// context to invoke, the private logical queue handle handed to the callback,
/// and the drain bookkeeping used to wait out callbacks already in flight.
struct MonitorReg {
    queue: Arc<Queue>,
    handle: usize,
    ctx: usize,
    cb: usize,
    state: Mutex<MonitorState>,
    drained: Condvar,
}

/// Live monitor registry, parallel in spirit to `WAITERS`: opaque monotonic
/// tokens (the shared `NEXT_TOKEN` sequence, so never colliding with waiter
/// tokens) → the registration. Claims are a single locked check-and-remove;
/// the lock is never held across user callbacks or drain waits.
static MONITORS: LazyLock<Mutex<HashMap<u64, Arc<MonitorReg>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

thread_local! {
    /// Registrations whose user callbacks are active on this thread. This is
    /// used only to avoid waiting for ourselves during reentrant unregister;
    /// the registration remains alive through the `Arc` held by the notifier.
    static ACTIVE_MONITORS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

fn monitor_is_active_here(reg: &Arc<MonitorReg>) -> bool {
    let key = Arc::as_ptr(reg) as usize;
    ACTIVE_MONITORS.with(|active| active.borrow().contains(&key))
}

fn finish_monitor_callback(reg: &Arc<MonitorReg>) {
    let retire = {
        let mut st = reg.state.lock().unwrap();
        st.inflight -= 1;
        let retire = st.inflight == 0 && !st.armed;
        if st.inflight == 0 {
            reg.drained.notify_all();
        }
        retire
    };
    if retire {
        LIVE.lock().unwrap().remove(&reg.handle);
    }
}

/// Notify every monitor armed on `q` that an item was submitted to `port`,
/// synchronously on the submitting thread. The map copy is the only part under
/// the registry lock; the armed/inflight billing that follows runs under the
/// registration's own lock, so neither lock is ever held across a user
/// callback, and an unregister racing this sweep either waits for the bill or
/// finds the monitor already closed – never both directions racing past.
fn notify_monitors(q: &Arc<Queue>, port: u32) {
    let armed: Vec<Arc<MonitorReg>> = {
        let g = MONITORS.lock().unwrap();
        g.values().filter(|m| Arc::ptr_eq(&m.queue, q)).cloned().collect()
    };
    for m in armed {
        let bill = {
            let mut st = m.state.lock().unwrap();
            if !st.armed {
                false // unregistered between the snapshot and this claim
            } else {
                st.inflight += 1;
                true
            }
        };
        if !bill {
            continue;
        }
        unsafe {
            let f: XTaskQueueMonitorCallback = std::mem::transmute(m.cb);
            let key = Arc::as_ptr(&m) as usize;
            ACTIVE_MONITORS.with(|active| active.borrow_mut().push(key));
            f(m.ctx as *mut c_void, m.handle as *mut c_void, port);
            ACTIVE_MONITORS.with(|active| {
                debug_assert_eq!(active.borrow_mut().pop(), Some(key));
            });
        }
        finish_monitor_callback(&m);
    }
}

pub unsafe extern "system" fn XTaskQueueRegisterMonitor(h: *mut c_void, ctx: *mut c_void, cb: usize, token: *mut RegistrationToken) -> HRESULT {
    if token.is_null() || cb == 0 {
        return E_INVALIDARG;
    }
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    // a private LIVE token as the callback's queue handle: it stays valid
    // however the game's registration handles are duplicated or closed, and
    // is retired only after unregister drains the last callback in flight
    let handle = new_handle(q.clone());
    let reg = Arc::new(MonitorReg {
        queue: q,
        handle: handle as usize,
        ctx: ctx as usize,
        cb,
        state: Mutex::new(MonitorState { armed: true, inflight: 0 }),
        drained: Condvar::new(),
    });
    // armed and recorded in one locked insertion; wraparound-safe like handles
    let mut g = MONITORS.lock().unwrap();
    loop {
        let id = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        if id <= 1 {
            continue;
        }
        if let std::collections::hash_map::Entry::Vacant(e) = g.entry(id) {
            e.insert(reg);
            (*token).token = id;
            return S_OK;
        }
    }
}

pub unsafe extern "system" fn XTaskQueueUnregisterMonitor(h: *mut c_void, token: RegistrationToken) {
    let Some(q) = queue_from(h) else { return };
    // a single locked claim: the token must be live AND armed for this queue.
    // Forged, stale, double-used, foreign (waiter-domain) and wrong-queue
    // tokens all miss – nothing is dereferenced, freed or unwound for them
    let reg = {
        let mut g = MONITORS.lock().unwrap();
        match g.entry(token.token) {
            std::collections::hash_map::Entry::Occupied(e) if Arc::ptr_eq(&e.get().queue, &q) => e.remove(),
            _ => return,
        }
    };
    // outside the registry lock: close `armed` under the same lock used by
    // notify's billing, then wait until callbacks already in flight finish.
    // A notifier snapshotting before the claim bills first and is awaited;
    // one snapshotting after finds armed = false and never calls. The game
    // callback itself may register/unregister/submit without deadlocking –
    // nobody holds MONITORS here.
    {
        let mut st = reg.state.lock().unwrap();
        st.armed = false;
        if monitor_is_active_here(&reg) {
            // Waiting here would wait for this very callback. Its notifier
            // owns `reg` and retires the private handle after the last active
            // callback (possibly on another thread) exits.
            return;
        }
        while st.inflight != 0 {
            st = reg.drained.wait(st).unwrap();
        }
    }
    // no callback is in flight and none can start: retire the private
    // callback-side queue handle now
    LIVE.lock().unwrap().remove(&reg.handle);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    static HITS: AtomicU32 = AtomicU32::new(0);
    static CANCELED: AtomicU32 = AtomicU32::new(0);

    unsafe extern "system" fn bump(ctx: *mut c_void, canceled: bool) {
        HITS.fetch_add(ctx as usize as u32, Ordering::SeqCst);
        if canceled {
            CANCELED.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn process_queue_state_distinguishes_default_disabled_and_replacement() {
        let mut state = ProcessQueueState { initialized: false, queue: None };
        let default = state.current().expect("default process queue was not materialized");
        assert_eq!(default.ports[PORT_WORK as usize].mode, MODE_THREAD_POOL);
        assert_eq!(default.ports[PORT_COMPLETION as usize].mode, MODE_THREAD_POOL);

        state.set(None);
        assert!(state.current().is_none(), "disabled process queue was silently recreated");

        let replacement = Queue::new(MODE_MANUAL, MODE_IMMEDIATE);
        state.set(Some(replacement.clone()));
        let current = state.current().expect("replacement process queue was lost");
        assert!(Arc::ptr_eq(&current, &replacement));
        drop(replacement);
        assert!(state.current().is_some(), "process queue did not retain its own reference");
    }

    #[test]
    fn manual_threadpool_immediate_delayed_and_terminate() {
        unsafe {
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_IMMEDIATE, &mut h), S_OK);
            let before = HITS.load(Ordering::SeqCst);
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, 1 as _, bump as usize), S_OK);
            assert_eq!(HITS.load(Ordering::SeqCst), before, "manual port must wait for dispatch");
            assert!(XTaskQueueDispatch(h, PORT_WORK, 0));
            assert_eq!(HITS.load(Ordering::SeqCst), before + 1);
            assert!(!XTaskQueueDispatch(h, PORT_WORK, 0));
            // immediate completion port runs inline
            XTaskQueueSubmitCallback(h, PORT_COMPLETION, 10 as _, bump as usize);
            assert_eq!(HITS.load(Ordering::SeqCst), before + 11);
            // delayed on manual port: dispatch with timeout waits for it
            XTaskQueueSubmitDelayedCallback(h, PORT_WORK, 30, 100 as _, bump as usize);
            assert!(XTaskQueueDispatch(h, PORT_WORK, 2000));
            assert_eq!(HITS.load(Ordering::SeqCst), before + 111);
            // terminate cancels pending manual work
            XTaskQueueSubmitCallback(h, PORT_WORK, 1000 as _, bump as usize);
            let c0 = CANCELED.load(Ordering::SeqCst);
            assert_eq!(XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0), S_OK);
            assert_eq!(CANCELED.load(Ordering::SeqCst), c0 + 1);
            XTaskQueueCloseHandle(h);

            // thread pool + serialized
            for mode in [MODE_THREAD_POOL, MODE_SERIALIZED_THREAD_POOL] {
                let mut t = std::ptr::null_mut();
                XTaskQueueCreate(mode, mode, &mut t);
                let b = HITS.load(Ordering::SeqCst);
                for _ in 0..50 {
                    XTaskQueueSubmitCallback(t, PORT_WORK, 1 as _, bump as usize);
                }
                XTaskQueueTerminate(t, true, std::ptr::null_mut(), 0);
                assert_eq!(HITS.load(Ordering::SeqCst), b + 50);
                XTaskQueueCloseHandle(t);
            }
        }
    }

    /// Per-test counters (leaked: the shared timer heap outlives the frame).
    /// Keeps these tests deterministic when the test harness runs them in
    /// parallel with the ones above that share HITS/CANCELED.
    struct Stats {
        hits: AtomicU32,
        canceled: AtomicU32,
        termed: AtomicU32,
        term_saw_hits: AtomicU32,
    }

    fn stats() -> &'static Stats {
        Box::leak(Box::new(Stats {
            hits: AtomicU32::new(0),
            canceled: AtomicU32::new(0),
            termed: AtomicU32::new(0),
            term_saw_hits: AtomicU32::new(0),
        }))
    }

    unsafe extern "system" fn stat_hit(ctx: *mut c_void, canceled: bool) {
        let s = &*(ctx as *const Stats);
        s.hits.fetch_add(1, Ordering::SeqCst);
        if canceled {
            s.canceled.fetch_add(1, Ordering::SeqCst);
        }
    }

    unsafe extern "system" fn stat_term(ctx: *mut c_void) {
        let s = &*(ctx as *const Stats);
        // by the time the termination callback runs, every submitted callback
        // (immediate, queued, running or delayed-canceled) must have finished
        s.term_saw_hits.store(s.hits.load(Ordering::SeqCst), Ordering::SeqCst);
        s.termed.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn terminate_wait_cancels_pending_timer_once() {
        unsafe {
            let s = stats();
            let sp = s as *const Stats as *mut c_void;
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
            let q = queue_from(h).unwrap();

            // one hour: far beyond anything terminate() may take - a version
            // that "waits for the timer" would hang here, not just lag
            assert_eq!(XTaskQueueSubmitDelayedCallback(h, PORT_WORK, 3_600_000, sp, stat_hit as usize), S_OK);
            assert!(!XTaskQueueIsEmpty(h, PORT_WORK), "a scheduled timer must count as in-flight");

            let t0 = Instant::now();
            assert_eq!(XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0), S_OK);
            assert!(t0.elapsed() < Duration::from_secs(2), "terminate(wait) blocked on a live timer");
            assert_eq!(s.hits.load(Ordering::SeqCst), 1, "delayed callback must run exactly once before return");
            assert_eq!(s.canceled.load(Ordering::SeqCst), 1, "and with canceled = true");
            // the entry is gone from the shared heap - it can never fire again
            let (m, _cv) = timer();
            assert!(
                m.lock().unwrap().iter().all(|t| !Arc::ptr_eq(&t.queue, &q)),
                "terminate left a timer pending for this queue"
            );
            XTaskQueueCloseHandle(h);
        }
    }

    #[test]
    fn canceled_timer_never_fires_after_original_deadline() {
        unsafe {
            let s = stats();
            let sp = s as *const Stats as *mut c_void;
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_SERIALIZED_THREAD_POOL, MODE_THREAD_POOL, &mut h), S_OK);
            assert_eq!(XTaskQueueSubmitDelayedCallback(h, PORT_WORK, 300, sp, stat_hit as usize), S_OK);
            assert!(!XTaskQueueIsEmpty(h, PORT_WORK));

            let t0 = Instant::now();
            assert_eq!(XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0), S_OK);
            assert!(t0.elapsed() < Duration::from_millis(250), "cancel must be prompt, not at the deadline");
            assert_eq!(s.hits.load(Ordering::SeqCst), 1);
            assert_eq!(s.canceled.load(Ordering::SeqCst), 1);

            // well past the 300 ms deadline: a leftover entry would fire here
            std::thread::sleep(Duration::from_millis(700));
            assert_eq!(s.hits.load(Ordering::SeqCst), 1, "callback fired again past its deadline");
            XTaskQueueCloseHandle(h);
        }
    }

    #[test]
    fn terminate_callback_runs_after_delayed_and_queued_work() {
        unsafe {
            let s = stats();
            let sp = s as *const Stats as *mut c_void;
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_THREAD_POOL, MODE_THREAD_POOL, &mut h), S_OK);
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, sp, stat_hit as usize), S_OK);
            assert_eq!(XTaskQueueSubmitDelayedCallback(h, PORT_WORK, 3_600_000, sp, stat_hit as usize), S_OK);

            // await = false: the callback runs on a pool thread, but only once
            // inflight drains - so after the immediate and the canceled
            // delayed callback both finished. The immediate callback may also
            // observe cancellation if the pool starts it after termination.
            assert_eq!(XTaskQueueTerminate(h, false, sp, stat_term as usize), S_OK);
            assert!(s.canceled.load(Ordering::SeqCst) >= 1, "pending timer was not canceled during terminate");

            let deadline = Instant::now() + Duration::from_secs(5);
            while s.termed.load(Ordering::SeqCst) == 0 {
                assert!(Instant::now() < deadline, "termination callback never ran");
                std::thread::yield_now();
            }
            assert_eq!(
                s.term_saw_hits.load(Ordering::SeqCst),
                2,
                "termination callback ran before all work finished"
            );
            XTaskQueueCloseHandle(h);
        }
    }

    #[test]
    fn terminate_drops_only_this_queues_timers() {
        unsafe {
            let sa = stats();
            let sb = stats();
            let mut ha = std::ptr::null_mut();
            let mut hb = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut ha), S_OK);
            assert_eq!(XTaskQueueCreate(MODE_THREAD_POOL, MODE_THREAD_POOL, &mut hb), S_OK);
            assert_eq!(XTaskQueueSubmitDelayedCallback(ha, PORT_WORK, 300, sa as *const Stats as *mut c_void, stat_hit as usize), S_OK);
            assert_eq!(XTaskQueueSubmitDelayedCallback(hb, PORT_WORK, 300, sb as *const Stats as *mut c_void, stat_hit as usize), S_OK);

            assert_eq!(XTaskQueueTerminate(ha, true, std::ptr::null_mut(), 0), S_OK);
            assert_eq!(sa.hits.load(Ordering::SeqCst), 1);
            assert_eq!(sa.canceled.load(Ordering::SeqCst), 1);

            // queue b is untouched: its timer still fires at the deadline,
            // uncanceled, while a's entry stays gone
            std::thread::sleep(Duration::from_millis(700));
            assert_eq!(sa.hits.load(Ordering::SeqCst), 1, "terminated queue's timer fired again");
            assert_eq!(sb.hits.load(Ordering::SeqCst), 1, "other queue's timer did not fire");
            assert_eq!(sb.canceled.load(Ordering::SeqCst), 0, "other queue's timer ran canceled");
            XTaskQueueCloseHandle(ha);
            XTaskQueueCloseHandle(hb);
        }
    }

    /// Forged, interior and unmapped pointers must be rejected by every
    /// exported entry point without being dereferenced.
    #[test]
    fn unregistered_pointers_are_rejected_without_deref() {
        unsafe {
            // An arbitrary stack address, an offset from a live token, and an
            // address that was never registered.
            let mut real = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut real), S_OK);
            let interior = (real as usize + 8) as *mut c_void;
            let mut planted = 0u8;
            let planted_ptr = &mut planted as *mut u8 as *mut c_void;
            let wild = 1usize as *mut c_void;

            for h in [planted_ptr, interior, wild] {
                assert!(queue_from(h).is_none(), "unregistered pointer resolved");
                let mut out = std::ptr::null_mut();
                assert_eq!(XTaskQueueDuplicateHandle(h, &mut out), E_INVALIDARG);
                assert!(out.is_null());
                assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), bump as usize), E_INVALIDARG);
                assert_eq!(XTaskQueueSubmitDelayedCallback(h, PORT_WORK, 0, std::ptr::null_mut(), bump as usize), E_INVALIDARG);
                assert_eq!(XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0), E_INVALIDARG);
                assert!(!XTaskQueueDispatch(h, PORT_WORK, 0));
                assert!(XTaskQueueIsEmpty(h, PORT_WORK));
                let mut tok = RegistrationToken { token: 0 };
                assert_eq!(
                    XTaskQueueRegisterWaiter(h, PORT_WORK, std::ptr::null_mut(), std::ptr::null_mut(), bump as usize, &mut tok),
                    E_INVALIDARG
                );
                assert_eq!(tok.token, 0);
                assert_eq!(XTaskQueueRegisterMonitor(h, std::ptr::null_mut(), bump as usize, &mut tok), E_INVALIDARG);
                XTaskQueueUnregisterMonitor(h, tok);
                // closing a pointer we never issued must free nothing
                XTaskQueueCloseHandle(h);
                XTaskQueueCloseHandle(h);
            }

            // probes at the interior address left the real handle untouched
            assert_eq!(
                XTaskQueueSubmitCallback(real, PORT_WORK, std::ptr::null_mut(), bump as usize),
                S_OK
            );
            assert!(XTaskQueueDispatch(real, PORT_WORK, 0));
            XTaskQueueCloseHandle(real);
            // forged values install no process queue; null still means it
            XTaskQueueSetCurrentProcessTaskQueue(planted_ptr);
            assert!(queue_from(std::ptr::null_mut()).is_some());
        }
    }

    /// Close claims the registration exactly once via an atomic remove; later
    /// closes of the same token are no-ops.
    #[test]
    fn close_is_atomic_and_idempotent() {
        unsafe {
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_THREAD_POOL, MODE_THREAD_POOL, &mut h), S_OK);
            assert!(queue_from(h).is_some());
            XTaskQueueCloseHandle(h);
            assert!(queue_from(h).is_none(), "closed handle still resolves");
            XTaskQueueCloseHandle(h);
            XTaskQueueCloseHandle(h);
            assert!(queue_from(h).is_none());
            // null remains a harmless no-op
            XTaskQueueCloseHandle(std::ptr::null_mut());
        }
    }

    /// Every API on a stale (previously valid, now closed) handle fails
    /// cleanly rather than touching reclaimed memory.
    #[test]
    fn stale_handle_after_close_fails_cleanly() {
        unsafe {
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
            XTaskQueueCloseHandle(h);
            let mut out = std::ptr::null_mut();
            assert_eq!(XTaskQueueDuplicateHandle(h, &mut out), E_INVALIDARG);
            assert!(out.is_null());
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), bump as usize), E_INVALIDARG);
            assert_eq!(XTaskQueueSubmitDelayedCallback(h, PORT_WORK, 0, std::ptr::null_mut(), bump as usize), E_INVALIDARG);
            assert_eq!(XTaskQueueTerminate(h, false, std::ptr::null_mut(), 0), E_INVALIDARG);
            assert!(!XTaskQueueDispatch(h, PORT_WORK, 0));
            assert!(XTaskQueueIsEmpty(h, PORT_WORK));
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterWaiter(h, PORT_WORK, std::ptr::null_mut(), std::ptr::null_mut(), bump as usize, &mut tok),
                E_INVALIDARG
            );
            XTaskQueueCloseHandle(h);
        }
    }

    /// Each handle is an independent registry entry over the shared queue:
    /// closing the original leaves duplicates fully usable.
    #[test]
    fn duplicate_handle_outlives_the_original() {
        unsafe {
            let s = stats();
            let sp = s as *const Stats as *mut c_void;
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
            let mut dup = std::ptr::null_mut();
            assert_eq!(XTaskQueueDuplicateHandle(h, &mut dup), S_OK);
            assert_ne!(dup, h, "duplicate must be a distinct handle");
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, sp, stat_hit as usize), S_OK);
            XTaskQueueCloseHandle(h);
            // the original rejects while the duplicate drives the same queue
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, sp, stat_hit as usize), E_INVALIDARG);
            assert_eq!(XTaskQueueSubmitCallback(dup, PORT_WORK, sp, stat_hit as usize), S_OK);
            assert!(XTaskQueueDispatch(dup, PORT_WORK, 0));
            assert!(XTaskQueueDispatch(dup, PORT_WORK, 0));
            assert_eq!(s.hits.load(Ordering::SeqCst), 2);
            // a duplicate of a duplicate stays valid while any sibling lives
            let mut dup2 = std::ptr::null_mut();
            assert_eq!(XTaskQueueDuplicateHandle(dup, &mut dup2), S_OK);
            XTaskQueueCloseHandle(dup);
            assert_eq!(XTaskQueueTerminate(dup2, true, std::ptr::null_mut(), 0), S_OK);
            XTaskQueueCloseHandle(dup2);
        }
    }

    /// Close racing concurrent handle use: every racing call resolves through
    /// the registry, so the reclaimed allocation is never touched – no use
    /// after free however the ordering lands.
    #[test]
    fn close_racing_handle_use_is_memory_safe() {
        unsafe {
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_THREAD_POOL, MODE_MANUAL, &mut h), S_OK);
            let s = stats();
            let sp = s as *const Stats as usize; // raw pointers are not Send
            let addr = h as usize;
            const THREADS: usize = 8;
            let barrier = Arc::new(std::sync::Barrier::new(THREADS + 1));
            let mut joins = Vec::new();
            for _ in 0..THREADS {
                let b = Arc::clone(&barrier);
                joins.push(
                    std::thread::Builder::new()
                        .spawn(move || {
                            b.wait();
                            let h = addr as *mut c_void;
                            for _ in 0..64 {
                                if XTaskQueueSubmitCallback(h, PORT_WORK, sp as *mut c_void, stat_hit as usize) == S_OK {
                                    XTaskQueueDispatch(h, PORT_WORK, 0);
                                }
                                let _ = XTaskQueueIsEmpty(h, PORT_WORK);
                                let mut out = std::ptr::null_mut();
                                if XTaskQueueDuplicateHandle(h, &mut out) == S_OK {
                                    XTaskQueueCloseHandle(out);
                                }
                            }
                        })
                        .unwrap(),
                );
            }
            barrier.wait();
            // freed mid-storm: callers either claimed the Arc first or miss
            XTaskQueueCloseHandle(h);
            for j in joins {
                j.join().unwrap();
            }
            assert!(queue_from(h).is_none());
            assert_eq!(
                XTaskQueueSubmitCallback(h, PORT_COMPLETION, sp as *mut c_void, stat_hit as usize),
                E_INVALIDARG
            );
        }
    }

    // ---------------------------------------------------------------------------------------
    // waiter registration safety (real Windows events)
    // ---------------------------------------------------------------------------------------

    /// An auto-reset event: each `SetEvent` arms exactly one wait completion
    /// and the object returns to non-signaled once the wait fires.
    unsafe fn test_event() -> *mut c_void {
        let ev = windows_sys::Win32::System::Threading::CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
        assert!(!ev.is_null(), "CreateEventW failed");
        ev
    }

    /// Signal, then drive the manual port until the queued callback runs.
    unsafe fn signal_and_dispatch(h: *mut c_void, ev: *mut c_void) -> bool {
        windows_sys::Win32::System::Threading::SetEvent(ev);
        XTaskQueueDispatch(h, PORT_WORK, 5000)
    }

    /// The waiter must survive forged, stale, replayed and foreign tokens and
    /// handles — and a real unregister must kill it exactly once.
    #[test]
    fn waiter_delivers_and_unregister_claims_exactly_once() {
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            let s = stats();
            let sp = s as *const Stats as *mut c_void;
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
            let ev = test_event();
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterWaiter(h, PORT_WORK, ev, sp, stat_hit as usize, &mut tok),
                S_OK
            );
            // opaque monotonic token – never the monitor token 1 or unset 0
            let t = tok.token;
            assert!(t > 1, "waiter token aliases a reserved value: {t}");
            assert!(signal_and_dispatch(h, ev), "signaled waiter never posted its callback");
            assert_eq!(s.hits.load(Ordering::SeqCst), 1);

            // forged + out-of-domain tokens are no-ops, including the monitor token 1
            for bad in [0u64, 1, t + 1, t.wrapping_add(0x5A5A), u64::MAX] {
                XTaskQueueUnregisterWaiter(h, RegistrationToken { token: bad });
            }
            // forged and stale queue handles cannot reach the waiter either
            let mut planted = 0u8;
            XTaskQueueUnregisterWaiter(&mut planted as *mut u8 as *mut c_void, RegistrationToken { token: t });
            let mut closed = std::ptr::null_mut();
            assert_eq!(XTaskQueueDuplicateHandle(h, &mut closed), S_OK);
            XTaskQueueCloseHandle(closed);
            XTaskQueueUnregisterWaiter(closed, RegistrationToken { token: t }); // stale handle, nothing resolves
            assert!(signal_and_dispatch(h, ev), "forged unregister killed the waiter");
            assert_eq!(s.hits.load(Ordering::SeqCst), 2);

            // any handle to the same queue carries the same association: the
            // remaining live duplicate can claim as well as the original
            let mut dup = std::ptr::null_mut();
            assert_eq!(XTaskQueueDuplicateHandle(h, &mut dup), S_OK);
            XTaskQueueUnregisterWaiter(dup, RegistrationToken { token: t });
            assert_eq!(windows_sys::Win32::System::Threading::SetEvent(ev), 1);
            assert!(!XTaskQueueDispatch(h, PORT_WORK, 400), "waiter posted after unregister");
            assert_eq!(s.hits.load(Ordering::SeqCst), 2);
            // stale + double claims neither crash nor double-free
            XTaskQueueUnregisterWaiter(dup, RegistrationToken { token: t });
            XTaskQueueUnregisterWaiter(h, RegistrationToken { token: t });

            // a new waiter gets a fresh strictly-greater token; the retired
            // value never aliases it
            let mut tok2 = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterWaiter(h, PORT_WORK, ev, sp, stat_hit as usize, &mut tok2),
                S_OK
            );
            assert!(tok2.token > t);
            XTaskQueueUnregisterWaiter(h, RegistrationToken { token: t }); // stale – must not kill tok2's waiter
            assert!(signal_and_dispatch(h, ev), "stale token unregistered tok2's waiter");
            assert_eq!(s.hits.load(Ordering::SeqCst), 3);
            XTaskQueueUnregisterWaiter(h, tok2);
            assert_eq!(CloseHandle(ev), 1);
            XTaskQueueCloseHandle(h);
            XTaskQueueCloseHandle(dup);
        }
    }

    /// Every bad argument – forged/invalid OS handle, out-of-range port, null
    /// callback, null output, forged queue – rejects before anything is armed
    /// and leaves the caller's token untouched.
    #[test]
    fn waiter_registration_validates_all_arguments() {
        unsafe {
            use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
            let s = stats();
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
            let ev = test_event();
            let mut tok = RegistrationToken { token: 0xDEAD_BEEF };

            // the original bug: port > 1 was accepted, then `submit_now`
            // indexed outside the 2-port array when the waiter fired
            for port in [2u32, u32::MAX] {
                assert_eq!(
                    XTaskQueueRegisterWaiter(h, port, ev, std::ptr::null_mut(), stat_hit as usize, &mut tok),
                    E_INVALIDARG
                );
                assert_eq!(tok.token, 0xDEAD_BEEF);
            }
            // null, pseudo and forged OS objects never reach the thread pool
            for os in [std::ptr::null_mut(), INVALID_HANDLE_VALUE, 0x1122_3344usize as *mut c_void] {
                assert_eq!(
                    XTaskQueueRegisterWaiter(h, PORT_WORK, os, std::ptr::null_mut(), stat_hit as usize, &mut tok),
                    E_INVALIDARG
                );
                assert_eq!(tok.token, 0xDEAD_BEEF);
            }
            // null callback and null output token
            assert_eq!(
                XTaskQueueRegisterWaiter(h, PORT_WORK, ev, std::ptr::null_mut(), 0, &mut tok),
                E_INVALIDARG
            );
            assert_eq!(
                XTaskQueueRegisterWaiter(h, PORT_WORK, ev, std::ptr::null_mut(), stat_hit as usize, std::ptr::null_mut()),
                E_INVALIDARG
            );
            // forged queue handle with otherwise-valid arguments
            let mut planted = 0u8;
            assert_eq!(
                XTaskQueueRegisterWaiter(
                    &mut planted as *mut u8 as *mut c_void,
                    PORT_WORK,
                    ev,
                    std::ptr::null_mut(),
                    stat_hit as usize,
                    &mut tok,
                ),
                E_INVALIDARG
            );
            // nothing was left armed behind the rejections
            assert_eq!(windows_sys::Win32::System::Threading::SetEvent(ev), 1);
            assert!(!XTaskQueueDispatch(h, PORT_WORK, 400));
            assert_eq!(s.hits.load(Ordering::SeqCst), 0);
            assert_eq!(CloseHandle(ev), 1);
            XTaskQueueCloseHandle(h);
        }
    }

    /// A token armed on queue A cannot be claimed through queue B's handle:
    /// the queue/token association is verified before removal.
    #[test]
    fn waiter_unregister_enforces_queue_association() {
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            let s = stats();
            let sp = s as *const Stats as *mut c_void;
            let mut ha = std::ptr::null_mut();
            let mut hb = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut ha), S_OK);
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut hb), S_OK);
            let ev = test_event();
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterWaiter(ha, PORT_WORK, ev, sp, stat_hit as usize, &mut tok),
                S_OK
            );
            // wrong queue, right token: the claim must miss and stay armed
            XTaskQueueUnregisterWaiter(hb, RegistrationToken { token: tok.token });
            assert!(WAITERS.lock().unwrap().contains_key(&tok.token));
            assert!(signal_and_dispatch(ha, ev), "wrong-queue unregister killed the waiter");
            assert_eq!(s.hits.load(Ordering::SeqCst), 1);
            // the owning queue claims; queue B is unaffected
            XTaskQueueUnregisterWaiter(ha, RegistrationToken { token: tok.token });
            assert!(WAITERS.lock().unwrap().get(&tok.token).is_none());
            assert_eq!(windows_sys::Win32::System::Threading::SetEvent(ev), 1);
            assert!(!XTaskQueueDispatch(ha, PORT_WORK, 400));
            assert_eq!(s.hits.load(Ordering::SeqCst), 1);
            assert_eq!(CloseHandle(ev), 1);
            XTaskQueueCloseHandle(ha);
            XTaskQueueCloseHandle(hb);
        }
    }

    /// Waiter and monitor tokens come from one monotonic sequence: each
    /// registration carries a token no registration of either kind can ever
    /// mint again, and a wrong-domain unregister always claims nothing.
    #[test]
    fn waiter_token_never_collides_with_monitor() {
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            let s = stats();
            let sp = s as *const Stats as *mut c_void;
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
            let ev = test_event();
            let mut mon = RegistrationToken { token: 0 };
            let ms = mon_stats();
            assert_eq!(
                XTaskQueueRegisterMonitor(h, ms as *const MonStats as *mut c_void, mon_hit as usize, &mut mon),
                S_OK
            );
            assert!(mon.token > 1, "monitor token aliases a reserved value: {}", mon.token);
            for _ in 0..4 {
                let mut w = RegistrationToken { token: 0 };
                assert_eq!(
                    XTaskQueueRegisterWaiter(h, PORT_WORK, ev, sp, stat_hit as usize, &mut w),
                    S_OK
                );
                assert_ne!(w.token, mon.token, "waiter token aliased the monitor token");
                assert!(w.token > 1);
                XTaskQueueUnregisterWaiter(h, w);
            }
            // unregistering the monitor token as a waiter frees nothing, and
            // unregistering a waiter token as a monitor is equally inert
            XTaskQueueUnregisterWaiter(h, mon);
            let mut w2 = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterWaiter(h, PORT_WORK, ev, sp, stat_hit as usize, &mut w2),
                S_OK
            );
            XTaskQueueUnregisterMonitor(h, w2); // foreign token – keeps the waiter armed
            assert!(signal_and_dispatch(h, ev), "monitor-domain unregister killed a waiter");
            XTaskQueueUnregisterWaiter(h, w2);
            // the monitor itself still fires until its own unregister claims it
            let before = ms.fired.load(Ordering::SeqCst);
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, sp, stat_hit as usize), S_OK);
            assert_eq!(ms.fired.load(Ordering::SeqCst), before + 1);
            XTaskQueueUnregisterMonitor(h, mon);
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, sp, stat_hit as usize), S_OK);
            assert_eq!(ms.fired.load(Ordering::SeqCst), before + 1, "monitor fired after unregister");
            while XTaskQueueDispatch(h, PORT_WORK, 0) {} // drain pending work
            assert_eq!(CloseHandle(ev), 1);
            XTaskQueueCloseHandle(h);
        }
    }

    /// Unregister racing a signaling storm and racing itself: the registry
    /// claim happens exactly once, in-flight callbacks are drained, and no
    /// signal after the claim can ever post — all without a double free.
    #[test]
    fn waiter_unregister_races_signaling_and_replays() {
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::SetEvent;
            let s = stats();
            let sp = s as *const Stats as *mut c_void;
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_THREAD_POOL, MODE_THREAD_POOL, &mut h), S_OK);
            let ev = test_event();
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterWaiter(h, PORT_WORK, ev, sp, stat_hit as usize, &mut tok),
                S_OK
            );
            let token = tok.token;
            let h_addr = h as usize;
            let ev_addr = ev as usize;
            const SIGNALERS: usize = 4;
            const CLAIMERS: usize = 8;
            let barrier = Arc::new(std::sync::Barrier::new(SIGNALERS + CLAIMERS + 1));
            let mut joins = Vec::new();
            for _ in 0..SIGNALERS {
                let b = Arc::clone(&barrier);
                joins.push(
                    std::thread::Builder::new()
                        .spawn(move || {
                            b.wait();
                            let ev = ev_addr as *mut c_void;
                            for _ in 0..400 {
                                SetEvent(ev);
                            }
                        })
                        .unwrap(),
                );
            }
            for _ in 0..CLAIMERS {
                let b = Arc::clone(&barrier);
                joins.push(
                    std::thread::Builder::new()
                        .spawn(move || {
                            b.wait();
                            for _ in 0..8 {
                                XTaskQueueUnregisterWaiter(h_addr as *mut c_void, RegistrationToken { token });
                            }
                        })
                        .unwrap(),
                );
            }
            barrier.wait();
            for j in joins {
                j.join().unwrap();
            }
            // claimed exactly once: the token is gone from the registry and a
            // stale replay on a still-valid handle stays a no-op
            assert!(WAITERS.lock().unwrap().get(&token).is_none());
            XTaskQueueUnregisterWaiter(h, RegistrationToken { token });
            // callbacks submitted before the claim may still be queued on our
            // pool – terminate(wait) drains them deterministically
            assert_eq!(XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0), S_OK);
            let settled = s.hits.load(Ordering::SeqCst);
            assert_eq!(SetEvent(ev), 1);
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(s.hits.load(Ordering::SeqCst), settled, "waiter posted after unregister");
            assert_eq!(CloseHandle(ev), 1);
            XTaskQueueCloseHandle(h);
        }
    }

    // ---------------------------------------------------------------------------------------
    // monitor notification (void(context, queue, port), synchronous at submit)
    // ---------------------------------------------------------------------------------------

    /// Per-test monitor counters (leaked: the worker pool outlives the frame).
    struct MonStats {
        fired: AtomicU32,
        in_cb: AtomicU32,
        work: AtomicU32,
        completion: AtomicU32,
        bad_port: AtomicU32,
        /// callbacks in which the queue handle resolved to the armed queue
        own_queue: AtomicU32,
        /// `Arc::as_ptr` of the armed queue; the test fills it in
        queue_ptr: AtomicUsize,
        /// the opaque queue handle last handed to the callback
        last_handle: AtomicUsize,
    }

    fn mon_stats() -> &'static MonStats {
        Box::leak(Box::new(MonStats {
            fired: AtomicU32::new(0),
            in_cb: AtomicU32::new(0),
            work: AtomicU32::new(0),
            completion: AtomicU32::new(0),
            bad_port: AtomicU32::new(0),
            own_queue: AtomicU32::new(0),
            queue_ptr: AtomicUsize::new(0),
            last_handle: AtomicUsize::new(0),
        }))
    }

    unsafe extern "system" fn mon_hit(ctx: *mut c_void, qh: *mut c_void, port: u32) {
        let s = &*(ctx as *const MonStats);
        s.fired.fetch_add(1, Ordering::SeqCst);
        s.in_cb.fetch_add(1, Ordering::SeqCst);
        match port {
            PORT_WORK => s.work.fetch_add(1, Ordering::SeqCst),
            PORT_COMPLETION => s.completion.fetch_add(1, Ordering::SeqCst),
            _ => s.bad_port.fetch_add(1, Ordering::SeqCst),
        };
        if queue_from(qh).map(|cq| Arc::as_ptr(&cq) as usize == s.queue_ptr.load(Ordering::SeqCst)).unwrap_or(false) {
            s.own_queue.fetch_add(1, Ordering::SeqCst);
        }
        s.last_handle.store(qh as usize, Ordering::SeqCst);
        s.in_cb.fetch_sub(1, Ordering::SeqCst);
    }

    unsafe extern "system" fn noop_item(_ctx: *mut c_void, _canceled: bool) {}

    /// Every dispatch mode, both ports: the notification is synchronous on the
    /// submitting thread – observable before `XTaskQueueSubmitCallback` even
    /// returns – and carries the submitted port plus a queue handle resolving
    /// to the armed queue.
    #[test]
    fn monitor_fires_synchronously_on_every_mode_and_port() {
        unsafe {
            for mode in [MODE_MANUAL, MODE_THREAD_POOL, MODE_SERIALIZED_THREAD_POOL, MODE_IMMEDIATE] {
                let s = mon_stats();
                let sp = s as *const MonStats as *mut c_void;
                let mut h = std::ptr::null_mut();
                assert_eq!(XTaskQueueCreate(mode, mode, &mut h), S_OK);
                s.queue_ptr.store(Arc::as_ptr(&queue_from(h).unwrap()) as usize, Ordering::SeqCst);
                let mut tok = RegistrationToken { token: 0 };
                assert_eq!(XTaskQueueRegisterMonitor(h, sp, mon_hit as usize, &mut tok), S_OK);
                assert!(tok.token > 1, "monitor token hit a reserved value: {}", tok.token);
                assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
                assert_eq!(s.fired.load(Ordering::SeqCst), 1, "mode {mode}: notify was not synchronous at submit");
                assert_eq!(s.work.load(Ordering::SeqCst), 1);
                assert_eq!(s.own_queue.load(Ordering::SeqCst), 1, "callback handle did not resolve to the armed queue");
                assert_ne!(s.last_handle.load(Ordering::SeqCst), h as usize, "callback got the registration handle, not the logical one");
                assert_eq!(XTaskQueueSubmitCallback(h, PORT_COMPLETION, std::ptr::null_mut(), noop_item as usize), S_OK);
                assert_eq!(s.fired.load(Ordering::SeqCst), 2);
                assert_eq!(s.completion.load(Ordering::SeqCst), 1);
                assert_eq!(s.own_queue.load(Ordering::SeqCst), 2);
                assert_eq!(s.bad_port.load(Ordering::SeqCst), 0);
                XTaskQueueUnregisterMonitor(h, tok);
                assert_eq!(s.in_cb.load(Ordering::SeqCst), 0);
                assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
                assert_eq!(s.fired.load(Ordering::SeqCst), 2, "monitor fired after unregister");
                XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0);
                XTaskQueueCloseHandle(h);
            }
        }
    }

    /// Failed submissions – bad port, null callback, forged handle – are not
    /// submissions for execution and never notify.
    #[test]
    fn monitor_never_fires_on_rejected_submissions() {
        unsafe {
            let s = mon_stats();
            let sp = s as *const MonStats as *mut c_void;
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
            s.queue_ptr.store(Arc::as_ptr(&queue_from(h).unwrap()) as usize, Ordering::SeqCst);
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(XTaskQueueRegisterMonitor(h, sp, mon_hit as usize, &mut tok), S_OK);
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), 0), E_INVALIDARG);
            assert_eq!(XTaskQueueSubmitCallback(h, 2, std::ptr::null_mut(), noop_item as usize), E_INVALIDARG);
            assert_eq!(XTaskQueueSubmitDelayedCallback(h, PORT_WORK, 0, std::ptr::null_mut(), 0), E_INVALIDARG);
            let mut planted = 0u8;
            assert_eq!(
                XTaskQueueSubmitCallback(&mut planted as *mut u8 as *mut c_void, PORT_WORK, std::ptr::null_mut(), noop_item as usize),
                E_INVALIDARG
            );
            assert_eq!(s.fired.load(Ordering::SeqCst), 0, "rejected submissions notified the monitor");
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
            assert_eq!(s.fired.load(Ordering::SeqCst), 1);
            XTaskQueueUnregisterMonitor(h, tok);
            XTaskQueueCloseHandle(h);
        }
    }

    /// A delayed item notifies exactly once – at submission, on the submitting
    /// thread, far before its deadline – and the timer promoting it onto the
    /// port at the deadline must not re-notify.
    #[test]
    fn delayed_notifies_once_at_submit_never_at_promotion() {
        unsafe {
            // manual port: submission fires the monitor while the item is
            // still parked; dispatching past the deadline fires nothing more
            let s = mon_stats();
            let sp = s as *const MonStats as *mut c_void;
            let st = stats();
            let stp = st as *const Stats as *mut c_void;
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
            s.queue_ptr.store(Arc::as_ptr(&queue_from(h).unwrap()) as usize, Ordering::SeqCst);
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(XTaskQueueRegisterMonitor(h, sp, mon_hit as usize, &mut tok), S_OK);
            let t0 = Instant::now();
            assert_eq!(XTaskQueueSubmitDelayedCallback(h, PORT_WORK, 400, stp, stat_hit as usize), S_OK);
            assert_eq!(s.fired.load(Ordering::SeqCst), 1, "delayed submission did not notify at submit");
            assert!(t0.elapsed() < Duration::from_millis(200), "notify was deferred to the deadline");
            assert_eq!(st.hits.load(Ordering::SeqCst), 0, "manual work ran before dispatch");
            assert!(XTaskQueueDispatch(h, PORT_WORK, 5000));
            assert_eq!(st.hits.load(Ordering::SeqCst), 1);
            assert_eq!(s.fired.load(Ordering::SeqCst), 1, "timer promotion re-notified the monitor");
            XTaskQueueUnregisterMonitor(h, tok);
            XTaskQueueCloseHandle(h);

            // threadpool port, delay == 0 falls through to submit_now: still
            // exactly one notification at submission
            let s2 = mon_stats();
            let st2 = stats();
            let mut h2 = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_THREAD_POOL, MODE_THREAD_POOL, &mut h2), S_OK);
            s2.queue_ptr.store(Arc::as_ptr(&queue_from(h2).unwrap()) as usize, Ordering::SeqCst);
            let mut tok2 = RegistrationToken { token: 0 };
            assert_eq!(XTaskQueueRegisterMonitor(h2, s2 as *const MonStats as *mut c_void, mon_hit as usize, &mut tok2), S_OK);
            assert_eq!(
                XTaskQueueSubmitDelayedCallback(h2, PORT_WORK, 0, std::ptr::null_mut(), noop_item as usize),
                S_OK
            );
            assert_eq!(s2.fired.load(Ordering::SeqCst), 1);
            assert_eq!(
                XTaskQueueSubmitDelayedCallback(h2, PORT_COMPLETION, 120, st2 as *const Stats as *mut c_void, stat_hit as usize),
                S_OK
            );
            assert_eq!(s2.fired.load(Ordering::SeqCst), 2);
            let deadline = Instant::now() + Duration::from_secs(5);
            while st2.hits.load(Ordering::SeqCst) == 0 {
                assert!(Instant::now() < deadline, "delayed completion-port item never ran");
                std::thread::yield_now();
            }
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(s2.fired.load(Ordering::SeqCst), 2, "promotion re-notified the monitor");
            XTaskQueueUnregisterMonitor(h2, tok2);
            XTaskQueueCloseHandle(h2);
        }
    }

    /// The monitor callback's queue handle is a private logical handle: it
    /// resolves to the armed queue even after the handle used at registration
    /// is duplicated and closed – and is retired only when unregister returns.
    #[test]
    fn monitor_callback_handle_outlives_registration_handle() {
        unsafe {
            let s = mon_stats();
            let sp = s as *const MonStats as *mut c_void;
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
            s.queue_ptr.store(Arc::as_ptr(&queue_from(h).unwrap()) as usize, Ordering::SeqCst);
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(XTaskQueueRegisterMonitor(h, sp, mon_hit as usize, &mut tok), S_OK);
            // duplicating then closing the registering handle must not strand
            // the callback's queue view
            let mut dup = std::ptr::null_mut();
            assert_eq!(XTaskQueueDuplicateHandle(h, &mut dup), S_OK);
            XTaskQueueCloseHandle(h);
            assert_eq!(XTaskQueueSubmitCallback(dup, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
            assert_eq!(s.fired.load(Ordering::SeqCst), 1);
            assert_eq!(s.own_queue.load(Ordering::SeqCst), 1, "callback handle died with the registration handle");
            let cbh = s.last_handle.load(Ordering::SeqCst);
            assert_ne!(cbh, 0);
            // any live handle carries the association: unregister through dup
            XTaskQueueUnregisterMonitor(dup, tok);
            // the callback-side handle is retired now, not at handle close
            assert!(queue_from(cbh as *mut c_void).is_none(), "callback handle outlived unregister");
            assert_eq!(XTaskQueueSubmitCallback(dup, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
            assert_eq!(s.fired.load(Ordering::SeqCst), 1);
            while XTaskQueueDispatch(dup, PORT_WORK, 0) {}
            XTaskQueueCloseHandle(dup);
        }
    }

    /// A monitor callback may itself submit work: the registry holds no lock
    /// while calling, so a bounded recursive submission notifies again without
    /// deadlocking.
    #[test]
    fn reentrant_submission_from_monitor_callback() {
        struct Reent {
            h: usize,
            resub: AtomicBool,
            fired: AtomicU32,
            submitted: AtomicU32,
        }
        unsafe extern "system" fn mon_reent(ctx: *mut c_void, _qh: *mut c_void, port: u32) {
            let r = &*(ctx as *const Reent);
            r.fired.fetch_add(1, Ordering::SeqCst);
            if !r.resub.swap(true, Ordering::SeqCst) {
                // submission from inside the notification: legal, and the very
                // same monitor is notified one more (bounded) time
                if XTaskQueueSubmitCallback(r.h as *mut c_void, port, std::ptr::null_mut(), noop_item as usize) == S_OK {
                    r.submitted.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        unsafe {
            let r = Box::leak(Box::new({
                let mut h = std::ptr::null_mut();
                assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
                Reent {
                    h: h as usize,
                    resub: AtomicBool::new(false),
                    fired: AtomicU32::new(0),
                    submitted: AtomicU32::new(0),
                }
            }));
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterMonitor(r.h as *mut c_void, r as *const Reent as *mut c_void, mon_reent as usize, &mut tok),
                S_OK
            );
            assert_eq!(
                XTaskQueueSubmitCallback(r.h as *mut c_void, PORT_WORK, std::ptr::null_mut(), noop_item as usize),
                S_OK
            );
            // two submissions (outer + reentrant), two notifications, zero deadlock
            assert_eq!(r.fired.load(Ordering::SeqCst), 2);
            assert_eq!(r.submitted.load(Ordering::SeqCst), 1);
            // both queued items dispatch normally afterwards
            assert!(XTaskQueueDispatch(r.h as *mut c_void, PORT_WORK, 0));
            assert!(XTaskQueueDispatch(r.h as *mut c_void, PORT_WORK, 0));
            assert!(!XTaskQueueDispatch(r.h as *mut c_void, PORT_WORK, 0));
            XTaskQueueUnregisterMonitor(r.h as *mut c_void, tok);
            XTaskQueueCloseHandle(r.h as *mut c_void);
        }
    }

    /// Every armed monitor is notified once per submission; unregistering one
    /// leaves the rest firing.
    #[test]
    fn multiple_monitors_each_notified_unregister_isolated() {
        unsafe {
            let sa = mon_stats();
            let sb = mon_stats();
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
            let qp = Arc::as_ptr(&queue_from(h).unwrap()) as usize;
            sa.queue_ptr.store(qp, Ordering::SeqCst);
            sb.queue_ptr.store(qp, Ordering::SeqCst);
            let mut ta = RegistrationToken { token: 0 };
            let mut tb = RegistrationToken { token: 0 };
            assert_eq!(XTaskQueueRegisterMonitor(h, sa as *const MonStats as *mut c_void, mon_hit as usize, &mut ta), S_OK);
            assert_eq!(XTaskQueueRegisterMonitor(h, sb as *const MonStats as *mut c_void, mon_hit as usize, &mut tb), S_OK);
            assert_ne!(ta.token, tb.token, "monitor tokens aliased");
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
            assert_eq!(sa.fired.load(Ordering::SeqCst), 1);
            assert_eq!(sb.fired.load(Ordering::SeqCst), 1);
            XTaskQueueUnregisterMonitor(h, ta);
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
            assert_eq!(sa.fired.load(Ordering::SeqCst), 1, "unregistered monitor fired");
            assert_eq!(sb.fired.load(Ordering::SeqCst), 2, "surviving monitor missed a submission");
            XTaskQueueUnregisterMonitor(h, tb);
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
            assert_eq!(sb.fired.load(Ordering::SeqCst), 2);
            while XTaskQueueDispatch(h, PORT_WORK, 0) {}
            XTaskQueueCloseHandle(h);
        }
    }

    /// Forged, stale, replayed, waiter-domain and wrong-queue token claims are
    /// all no-ops; only a live token presented on the armed queue claims.
    #[test]
    fn unregister_monitor_survives_forged_wrong_queue_and_replays() {
        unsafe {
            let sa = mon_stats();
            let sb = mon_stats();
            let mut ha = std::ptr::null_mut();
            let mut hb = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut ha), S_OK);
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut hb), S_OK);
            sa.queue_ptr.store(Arc::as_ptr(&queue_from(ha).unwrap()) as usize, Ordering::SeqCst);
            sb.queue_ptr.store(Arc::as_ptr(&queue_from(hb).unwrap()) as usize, Ordering::SeqCst);
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterMonitor(ha, sa as *const MonStats as *mut c_void, mon_hit as usize, &mut tok),
                S_OK
            );
            let t = tok.token;
            // out-of-domain and replay-forged claims
            for bad in [0u64, 1, t + 1, t.wrapping_add(0x5A5A), u64::MAX] {
                XTaskQueueUnregisterMonitor(ha, RegistrationToken { token: bad });
            }
            // wrong queue, right token
            XTaskQueueUnregisterMonitor(hb, RegistrationToken { token: t });
            // a stale queue handle cannot reach it either
            let mut dead = std::ptr::null_mut();
            assert_eq!(XTaskQueueDuplicateHandle(ha, &mut dead), S_OK);
            XTaskQueueCloseHandle(dead);
            XTaskQueueUnregisterMonitor(dead, tok);
            // still armed
            assert_eq!(XTaskQueueSubmitCallback(ha, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
            assert_eq!(sa.fired.load(Ordering::SeqCst), 1, "forged or wrong-queue claims killed the monitor");
            // a monitor on b is isolated from a's claims
            let mut tokb = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterMonitor(hb, sb as *const MonStats as *mut c_void, mon_hit as usize, &mut tokb),
                S_OK
            );
            XTaskQueueUnregisterMonitor(ha, tokb); // b's token through a's handle: no-op
            assert_eq!(XTaskQueueSubmitCallback(hb, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
            assert_eq!(sb.fired.load(Ordering::SeqCst), 1);
            // the real claim succeeds exactly once; replays are inert
            XTaskQueueUnregisterMonitor(ha, tok);
            XTaskQueueUnregisterMonitor(ha, tok);
            XTaskQueueUnregisterMonitor(hb, tok); // right token, wrong queue, now also stale
            assert_eq!(XTaskQueueSubmitCallback(ha, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
            assert_eq!(sa.fired.load(Ordering::SeqCst), 1);
            // a fresh registration mints a strictly-greater token; the retired
            // value can never be revived against it
            let mut tok2 = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterMonitor(ha, sa as *const MonStats as *mut c_void, mon_hit as usize, &mut tok2),
                S_OK
            );
            assert!(tok2.token > t);
            XTaskQueueUnregisterMonitor(ha, RegistrationToken { token: t });
            assert_eq!(XTaskQueueSubmitCallback(ha, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
            assert_eq!(sa.fired.load(Ordering::SeqCst), 2, "stale token claimed the new monitor");
            XTaskQueueUnregisterMonitor(ha, tok2);
            XTaskQueueUnregisterMonitor(hb, tokb);
            while XTaskQueueDispatch(ha, PORT_WORK, 0) {}
            while XTaskQueueDispatch(hb, PORT_WORK, 0) {}
            XTaskQueueCloseHandle(ha);
            XTaskQueueCloseHandle(hb);
        }
    }

    /// A callback may unregister its own registration without waiting for
    /// itself. The registration disappears immediately, while its private
    /// queue handle remains valid until the callback returns.
    #[test]
    fn monitor_can_unregister_itself() {
        struct SelfUnregister {
            queue: AtomicUsize,
            token: AtomicU64,
            callback_handle: AtomicUsize,
            fired: AtomicU32,
            returned: AtomicBool,
            handle_live_after_unregister: AtomicBool,
        }
        unsafe extern "system" fn unregister_self(ctx: *mut c_void, qh: *mut c_void, _port: u32) {
            let s = &*(ctx as *const SelfUnregister);
            s.fired.fetch_add(1, Ordering::SeqCst);
            s.callback_handle.store(qh as usize, Ordering::SeqCst);
            XTaskQueueUnregisterMonitor(
                s.queue.load(Ordering::SeqCst) as *mut c_void,
                RegistrationToken { token: s.token.load(Ordering::SeqCst) },
            );
            s.returned.store(true, Ordering::SeqCst);
            s.handle_live_after_unregister.store(queue_from(qh).is_some(), Ordering::SeqCst);
        }

        unsafe {
            let s = Box::leak(Box::new(SelfUnregister {
                queue: AtomicUsize::new(0),
                token: AtomicU64::new(0),
                callback_handle: AtomicUsize::new(0),
                fired: AtomicU32::new(0),
                returned: AtomicBool::new(false),
                handle_live_after_unregister: AtomicBool::new(false),
            }));
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
            s.queue.store(h as usize, Ordering::SeqCst);
            let mut token = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterMonitor(h, s as *const SelfUnregister as *mut c_void, unregister_self as usize, &mut token),
                S_OK
            );
            s.token.store(token.token, Ordering::SeqCst);

            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
            assert!(s.returned.load(Ordering::SeqCst), "self-unregister did not return");
            assert!(s.handle_live_after_unregister.load(Ordering::SeqCst), "callback handle retired before callback exit");
            assert_eq!(s.fired.load(Ordering::SeqCst), 1);
            let callback_handle = s.callback_handle.load(Ordering::SeqCst) as *mut c_void;
            assert!(queue_from(callback_handle).is_none(), "callback handle survived callback exit");

            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
            assert_eq!(s.fired.load(Ordering::SeqCst), 1, "self-unregistered monitor fired again");
            while XTaskQueueDispatch(h, PORT_WORK, 0) {}
            XTaskQueueCloseHandle(h);
        }
    }

    /// Unregister blocks until a monitor callback already in flight finishes:
    /// the callback gates on a latch released by a third thread, and the claim
    /// cannot return before the callback observed it.
    #[test]
    fn unregister_blocks_until_inflight_callback_finishes() {
        struct Gate {
            entered: AtomicU32,
            release: AtomicBool,
            finished: AtomicU32,
        }
        unsafe extern "system" fn mon_gate(ctx: *mut c_void, _qh: *mut c_void, _port: u32) {
            let g = &*(ctx as *const Gate);
            g.entered.fetch_add(1, Ordering::SeqCst);
            while !g.release.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            g.finished.fetch_add(1, Ordering::SeqCst);
        }
        unsafe {
            let g = Box::leak(Box::new(Gate {
                entered: AtomicU32::new(0),
                release: AtomicBool::new(false),
                finished: AtomicU32::new(0),
            }));
            let gp = g as *const Gate as *mut c_void;
            let gp_addr = gp as usize;
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(XTaskQueueRegisterMonitor(h, gp, mon_gate as usize, &mut tok), S_OK);
            // notifier runs on the submitting thread: submit from a helper so
            // its callback spins inside, then claim mid-flight on this thread
            let h_addr = h as usize;
            let submitter = std::thread::spawn(move || {
                XTaskQueueSubmitCallback(h_addr as *mut c_void, PORT_WORK, std::ptr::null_mut(), noop_item as usize)
            });
            let releaser = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                let g = &*(gp_addr as *const Gate);
                g.release.store(true, Ordering::SeqCst);
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while g.entered.load(Ordering::SeqCst) == 0 {
                assert!(Instant::now() < deadline, "monitor callback never started");
                std::thread::yield_now();
            }
            XTaskQueueUnregisterMonitor(h, tok); // must wait out the callback
            assert_eq!(g.finished.load(Ordering::SeqCst), 1, "unregister returned while a callback was in flight");
            submitter.join().unwrap();
            releaser.join().unwrap();
            while XTaskQueueDispatch(h, PORT_WORK, 0) {}
            XTaskQueueCloseHandle(h);
        }
    }

    /// A submit storm racing unregister: whatever callbacks were billed in
    /// flight are drained by the unregister return, and no notification can be
    /// observed afterwards – including for submissions issued later.
    #[test]
    fn unregister_racing_submit_storm_never_fires_after_return() {
        unsafe {
            let s = mon_stats();
            let sp = s as *const MonStats as usize;
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_THREAD_POOL, MODE_THREAD_POOL, &mut h), S_OK);
            s.queue_ptr.store(Arc::as_ptr(&queue_from(h).unwrap()) as usize, Ordering::SeqCst);
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(XTaskQueueRegisterMonitor(h, sp as *mut c_void, mon_hit as usize, &mut tok), S_OK);
            let h_addr = h as usize;
            let stop = Arc::new(AtomicBool::new(false));
            const SUBMITTERS: usize = 4;
            let barrier = Arc::new(std::sync::Barrier::new(SUBMITTERS + 1));
            let mut joins = Vec::new();
            for _ in 0..SUBMITTERS {
                let b = Arc::clone(&barrier);
                let stop = Arc::clone(&stop);
                joins.push(
                    std::thread::Builder::new()
                        .spawn(move || {
                            b.wait();
                            while !stop.load(Ordering::SeqCst) {
                                XTaskQueueSubmitCallback(h_addr as *mut c_void, PORT_WORK, std::ptr::null_mut(), noop_item as usize);
                            }
                        })
                        .unwrap(),
                );
            }
            barrier.wait();
            // let the storm actually hit the monitor before claiming
            let deadline = Instant::now() + Duration::from_secs(5);
            while s.fired.load(Ordering::SeqCst) < 32 {
                assert!(Instant::now() < deadline, "storm never reached the monitor");
                std::thread::yield_now();
            }
            XTaskQueueUnregisterMonitor(h, tok);
            // at return every billed callback has finished…
            assert_eq!(s.in_cb.load(Ordering::SeqCst), 0, "unregister returned with callbacks in flight");
            let frozen = s.fired.load(Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(s.fired.load(Ordering::SeqCst), frozen, "callback ran after unregister returned");
            stop.store(true, Ordering::SeqCst);
            for j in joins {
                j.join().unwrap();
            }
            // …and submissions after the claim notify nothing at all
            for _ in 0..8 {
                assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), noop_item as usize), S_OK);
            }
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(s.fired.load(Ordering::SeqCst), frozen, "post-unregister submissions re-notified the monitor");
            assert_eq!(XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0), S_OK);
            XTaskQueueCloseHandle(h);
        }
    }
}
