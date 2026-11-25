//! Debugging (`XCL_SUBMIT_DIAG=1`): what the last submissions of each queue held – per PC list
//! its draws, dispatches, ExecuteIndirects and the record hashes of its compute programs, and
//! the `Write64` values it signals – logged when a bridged GPU wait times out, to name the work
//! a queue that stops completing is executing.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

/// submissions kept per queue
const KEEP: usize = 12;
/// distinct compute records kept per list
const RECORDS: usize = 24;

#[derive(Clone, Copy)]
pub enum Kind {
    Draw,
    Dispatch,
    /// with its MaxCommandCount and whether a count buffer limits it
    ExecuteIndirect(u32, bool),
}

#[derive(Default, Clone)]
struct ListDiag {
    draws: u32,
    dispatches: u32,
    indirect: u32,
    /// largest MaxCommandCount of an ExecuteIndirect without a count buffer / with one
    ei_max: (u32, u32),
    /// (record hash or PSO pointer, count)
    records: Vec<(u64, u32)>,
}

struct Submission {
    seq: u64,
    at_ms: u128,
    lists: Vec<(usize, ListDiag, Vec<(u64, u64)>)>,
}

static LISTS: Mutex<Option<HashMap<usize, ListDiag>>> = Mutex::new(None);
static QUEUES: Mutex<Option<HashMap<usize, VecDeque<Submission>>>> = Mutex::new(None);

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("XCL_SUBMIT_DIAG").is_ok_and(|v| v.trim() == "1"))
}

fn t0() -> std::time::Instant {
    static T: OnceLock<std::time::Instant> = OnceLock::new();
    *T.get_or_init(std::time::Instant::now)
}

/// `list` (a PC list being recorded) got a draw / dispatch / ExecuteIndirect of `pso`.
pub fn note(list: *mut c_void, kind: Kind, pso: usize) {
    if !enabled() {
        return;
    }
    let rec = super::real::pso_record_of(pso).unwrap_or(pso as u64);
    let mut g = LISTS.lock().unwrap();
    let d = g.get_or_insert_with(HashMap::new).entry(list as usize).or_default();
    match kind {
        Kind::Draw => d.draws += 1,
        Kind::Dispatch => d.dispatches += 1,
        Kind::ExecuteIndirect(max, counted) => {
            d.indirect += 1;
            if counted {
                d.ei_max.1 = d.ei_max.1.max(max);
            } else {
                d.ei_max.0 = d.ei_max.0.max(max);
            }
        }
    }
    if let Some(e) = d.records.iter_mut().find(|e| e.0 == rec) {
        e.1 += 1;
    } else if d.records.len() < RECORDS {
        d.records.push((rec, 1));
    }
}

/// `lists` were submitted to `queue` (one ExecuteCommandLists chunk); `signals(list)`: the
/// `Write64` (address, value) pairs the list signals.
pub fn submitted(queue: *mut c_void, lists: &[*mut c_void], signals: impl Fn(*mut c_void) -> Vec<(u64, u64)>) {
    if !enabled() {
        return;
    }
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let taken: Vec<(usize, ListDiag, Vec<(u64, u64)>)> = {
        let mut g = LISTS.lock().unwrap();
        let m = g.get_or_insert_with(HashMap::new);
        lists.iter().map(|&l| (l as usize, m.remove(&(l as usize)).unwrap_or_default(), signals(l))).collect()
    };
    let s = Submission { seq: SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed), at_ms: t0().elapsed().as_millis(), lists: taken };
    let mut g = QUEUES.lock().unwrap();
    let q = g.get_or_insert_with(HashMap::new).entry(queue as usize).or_default();
    if q.len() == KEEP {
        q.pop_front();
    }
    q.push_back(s);
}

/// Log the kept submissions of every queue (at most `4` times a run).
pub fn dump(why: &str) {
    if !enabled() {
        return;
    }
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 4 {
        return;
    }
    let now = t0().elapsed().as_millis();
    let g = QUEUES.lock().unwrap();
    let Some(m) = g.as_ref() else { return };
    crate::xlog_warn!("submit diag ({why}, at {now} ms): the last {KEEP} submissions of {} queue(s)", m.len());
    for (&q, subs) in m {
        for s in subs {
            let lists: Vec<String> = s
                .lists
                .iter()
                .map(|(l, d, sig)| {
                    let recs: Vec<String> = d.records.iter().map(|(r, n)| format!("{r:016x}x{n}")).collect();
                    let sigs: Vec<String> = sig.iter().map(|(a, v)| format!("{a:#x}={v:#x}")).collect();
                    format!("[list {l:#x}: {} draws, {} dispatches, {} EI (max count {} / {} with a count buffer); signals {}; {}]", d.draws, d.dispatches, d.indirect, d.ei_max.0, d.ei_max.1, sigs.join(","), recs.join(" "))
                })
                .collect();
            crate::xlog_warn!("submit diag: q{} #{} at {} ms ({} ms ago), {} list(s): {}", super::list_x::queue_id_of(q), s.seq, s.at_ms, now - s.at_ms, s.lists.len(), lists.join(" "));
        }
    }
}
