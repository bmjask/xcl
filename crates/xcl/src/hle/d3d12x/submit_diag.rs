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
    lists: Vec<(usize, ListDiag, Vec<(u64, u64)>, String)>,
}

static LISTS: Mutex<Option<HashMap<usize, ListDiag>>> = Mutex::new(None);
static QUEUES: Mutex<Option<HashMap<usize, VecDeque<Submission>>>> = Mutex::new(None);

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    #[cfg(test)]
    if FORCE.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    *ON.get_or_init(|| std::env::var("XCL_SUBMIT_DIAG").is_ok_and(|v| v.trim() == "1"))
}

#[cfg(test)]
static FORCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
    // (only the entries under the lock: `signals` and `origin` take other modules' locks –
    // `console_list`'s, held while its replay calls `note`)
    let diags: Vec<ListDiag> = {
        let mut g = LISTS.lock().unwrap();
        let m = g.get_or_insert_with(HashMap::new);
        lists.iter().map(|&l| m.remove(&(l as usize)).unwrap_or_default()).collect()
    };
    let taken: Vec<(usize, ListDiag, Vec<(u64, u64)>, String)> = lists.iter().zip(diags).map(|(&l, d)| (l as usize, d, signals(l), origin(l))).collect();
    let s = Submission { seq: SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed), at_ms: t0().elapsed().as_millis(), lists: taken };
    let mut g = QUEUES.lock().unwrap();
    let q = g.get_or_insert_with(HashMap::new).entry(queue as usize).or_default();
    if q.len() == KEEP {
        q.pop_front();
    }
    q.push_back(s);
}

/// Where a submitted PC list comes from: a forwarded title list (its primary list or a split
/// segment, `title_list`), a console list's replay, or neither (a PC list of the title's own).
fn origin(l: *mut c_void) -> String {
    match super::title_list::wrapper_of(l) {
        Some(w) => {
            let mut segs = Vec::new();
            super::title_list::push_segments(w, &mut segs);
            match segs.iter().position(|&s| s == l) {
                Some(0) => format!("title list {:#x}, primary", w as usize),
                Some(k) => format!("title list {:#x}, segment {k} of {}", w as usize, segs.len()),
                None => format!("title list {:#x}, not among its segments", w as usize),
            }
        }
        None if super::console_list::is_replay_list(l) => "console list replay".to_string(),
        None => "not a title list".to_string(),
    }
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
                .map(|(l, d, sig, from)| {
                    let recs: Vec<String> = d.records.iter().map(|(r, n)| format!("{r:016x}x{n}")).collect();
                    let sigs: Vec<String> = sig.iter().map(|(a, v)| format!("{a:#x}={v:#x}")).collect();
                    format!("[list {l:#x} ({from}): {} draws, {} dispatches, {} EI (max count {} / {} with a count buffer); signals {}; {}]", d.draws, d.dispatches, d.indirect, d.ei_max.0, d.ei_max.1, sigs.join(","), recs.join(" "))
                })
                .collect();
            crate::xlog_warn!("submit diag: q{} #{} at {} ms ({} ms ago), {} list(s): {}", super::list_x::queue_id_of(q), s.seq, s.at_ms, now - s.at_ms, s.lists.len(), lists.join(" "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `submitted` looks up a list's origin (`console_list`'s lock) while a console list's
    /// replay, holding that lock, records dispatches (`note`, this module's lock): neither may
    /// wait for the other.
    #[test]
    fn submitted_does_not_hold_its_lock_while_asking_console_list() {
        FORCE.store(true, std::sync::atomic::Ordering::Relaxed);
        let list = 0x5d1a_0001usize;
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (a_tx, a_rx) = std::sync::mpsc::channel();
        let (b_tx, b_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            super::super::console_list::with_lists_locked_for_test(|| {
                ready_tx.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(300));
                note(list as *mut c_void, Kind::Dispatch, 0);
            });
            a_tx.send(()).unwrap();
        });
        ready_rx.recv().unwrap();
        std::thread::spawn(move || {
            submitted(0x5d1a_0100usize as *mut c_void, &[list as *mut c_void], |_| Vec::new());
            b_tx.send(()).unwrap();
        });
        let t = std::time::Duration::from_secs(5);
        let a = a_rx.recv_timeout(t).is_ok();
        let b = b_rx.recv_timeout(t).is_ok();
        FORCE.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(a && b, "note (under console_list's lock) and submitted waited for each other: note done {a}, submitted done {b}");
    }
}
