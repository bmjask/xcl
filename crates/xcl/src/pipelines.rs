//! `xcl pipelines`: compiles a title's pipelines in its own process, from the journal a run
//! writes (`hle::d3d12x::pso_journal`): the DXIL cache and the GPU driver's cache get what the
//! title asks for, so its pipelines are ready when it binds them – before the next run, or
//! during this one (`--watch` follows the journal as the title writes it, at below-normal
//! priority). Pipelines the title used come first. What is done is kept next to the journal
//! (`pipelines.done`, for this native preparation layout), so a stopped run goes on where it was.
//! The offline tool and title must use the same translation settings and driver cache.

pub(crate) mod warm_replay;

use crate::hle::d3d12x::pso_journal::{self, Replay};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

const HELP: &str = "\
xcl pipelines <game folder | title data folder | pipelines.bin> [--watch] [--threads N] [--limit N]
  Compiles the pipelines a run of the title asked for (<data>\\cache\\pipelines.bin) outside
  the title: the DXIL cache and the driver's cache. --watch: also while the title runs,
  following what it adds (stop with Ctrl+C). --threads: pipelines at once (default: half the
  cores, at below-normal priority; XCL_PIPELINE_THREADS caps the driver compiles, default 4, 2 with --watch).";

/// The journal of `target`: the file itself, a data folder with one, or a game folder.
fn journal_of(target: &Path) -> Result<PathBuf, String> {
    if target.is_file() && target.extension().is_some_and(|e| e.eq_ignore_ascii_case("bin")) {
        return Ok(target.to_path_buf());
    }
    let own = pso_journal::path_in(target);
    if own.is_file() {
        return Ok(own);
    }
    let (_, _, title) = crate::launcher::resolve_location(target)?;
    let j = pso_journal::path_in(&crate::launcher::data_root().join(&title));
    if j.is_file() {
        Ok(j)
    } else {
        Err(format!("no pipeline journal at {} – run the title once with this XCL (`xcl run`) to write it", j.display()))
    }
}

/// The native preparation layout on this GPU and driver (`gpu`): what `pipelines.done` is
/// valid for. A diagnostics-only rebuild keeps the checkpoint; a layout or driver change does not.
fn offline_key(gpu: u64) -> u64 {
    let source = u64::from_str_radix(env!("XCL_NATIVE_PREPARATION_ID"), 16).unwrap_or(0);
    let dir = crate::gpu::recomp::dxc::cache_dir();
    let dir = std::path::absolute(&dir).unwrap_or(dir);
    checkpoint_key(source, crate::shaders::warm_settings_key(), gpu, &dir,
        crate::gpu::recomp::pso::translation_cache_identity(), crate::gpu::recomp::pso::split_wave64_forced(),
        std::env::var("XCL_PSO_DISK_CACHE").map_or(true, |s| s.trim() != "0"))
}

pub(crate) fn native_key(dev: &windows::Win32::Graphics::Direct3D12::ID3D12Device) -> Option<u64> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::*;
    let gpu = unsafe {
        let f: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
        let adapter: IDXGIAdapter1 = f.EnumAdapterByLuid(dev.GetAdapterLuid()).ok()?;
        let desc = adapter.GetDesc1().ok()?;
        let driver = adapter.CheckInterfaceSupport(&IDXGIDevice::IID).ok()?;
        if driver == 0 { return None; }
        let parts = [desc.VendorId as u64, desc.DeviceId as u64, desc.SubSysId as u64, desc.Revision as u64, driver as u64];
        crate::gpu::shader::fnv64(&parts.map(u64::to_le_bytes).concat())
    };
    let source = u64::from_str_radix(env!("XCL_NATIVE_PREPARATION_ID"), 16).ok()?;
    let translation = crate::gpu::recomp::pso::translation_cache_identity();
    if source == 0 || translation == 0 { return None; }
    let cache = std::path::absolute(crate::gpu::recomp::dxc::cache_dir()).ok()?;
    let settings = crate::shaders::warm_settings_key();
    let state = checkpoint_key(source, settings, gpu, &cache, translation, crate::gpu::recomp::pso::split_wave64_forced(), true);
    // Keep versioned startup proofs separate from older executable-specific offline checkpoints.
    let mut proof = b"xcl-launch-proof-v1".to_vec();
    proof.extend_from_slice(&state.to_le_bytes());
    Some(crate::gpu::shader::fnv64(&proof))
}

fn checkpoint_key(source: u64, settings: u64, gpu: u64, cache: &Path, translation: u64, split: bool, disk: bool) -> u64 {
    let mut bytes = b"pipeline-checkpoint-v2".to_vec();
    for x in [source, settings, gpu, translation] { bytes.extend_from_slice(&x.to_le_bytes()); }
    bytes.extend_from_slice(&[split as u8, disk as u8]);
    bytes.extend_from_slice(cache.as_os_str().as_encoded_bytes());
    crate::gpu::shader::fnv64(&bytes)
}

/// A worker's stack: the driver's compile of a title's biggest compute programs (up to ~1.2 MB of
/// DXIL) runs on it, and a translation layer's (vkd3d-proton under Wine) went past 64 MB.
/// Address space only: pages are committed as used.
const WARM_STACK: usize = 512 << 20;

/// This run's `pipelines.trying`, removed on Ctrl+C, Ctrl+Break or closing the console
/// (`on_console_close`).
static TRYING_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The pipelines being made when the user stops the tool (Ctrl+C is how `--watch` ends) were cut
/// short, not crashed on: without `.trying` the next run makes them again. FALSE: the default
/// handler then ends the process. (std opens files with FILE_SHARE_DELETE, so the open handle
/// does not keep it.)
unsafe extern "system" fn on_console_close(_kind: u32) -> i32 {
    if let Some(p) = TRYING_PATH.get() {
        let _ = std::fs::remove_file(p);
    }
    0
}

/// `pipelines.trying`: the pipelines in the making now, written whole at each change, so a run cut
/// short (a crash, or Ctrl+C) leaves the next run exactly those.
struct Trying {
    file: std::fs::File,
    now: Vec<u64>,
}

impl Trying {
    fn set(&mut self, hash: u64, on: bool) {
        if on {
            self.now.push(hash);
        } else {
            self.now.retain(|&h| h != hash);
        }
        let bytes: Vec<u8> = self.now.iter().flat_map(|h| h.to_le_bytes()).collect();
        let _ = self.file.set_len(0).and_then(|_| self.file.rewind()).and_then(|_| self.file.write_all(&bytes));
    }
}

#[derive(Default)]
struct Queue {
    prepare_numbered_mesh: bool,
    /// pipelines the title built, then the others (journal order)
    used: VecDeque<u64>,
    demanded: VecDeque<u64>,
    repeats: HashMap<u64, HashSet<u32>>,
    rest: VecDeque<u64>,
    /// done, or taken by a worker
    taken: HashSet<u64>,
    shaders: HashMap<u64, Arc<Vec<u8>>>,
    mesh_sources: HashMap<u64, bool>,
    pipes: HashMap<u64, (u8, Arc<Vec<u8>>)>,
    was_used: HashSet<u64>,
    /// the memory windows of the run (`run` sets them: `pso::set_mem_windows`); other runs' are left out
    windows: u8,
    other_windows: usize,
    /// the journal: bytes read, and where its whole frames end
    buf: Vec<u8>,
    parsed: usize,
    closed: bool,
}

/// Compile every captured recipe for these memory windows on the actual render device.
/// Driver objects are transferred individually when the guest requests the corresponding PSO.
pub(crate) fn prepare_before_entry(data: &Path, progress: &crate::shaders::LaunchPreparation,
    prepare_shaders: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    let path = pso_journal::path_in(data);
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => Some(file),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("Reading {}: {e}", path.display())),
    };
    if crate::state::config().graphics.eq_ignore_ascii_case("null") { return prepare_shaders(); }
    let mut q = Queue {
        prepare_numbered_mesh: pso_journal::numbered_mesh_mode() == pso_journal::NumberedMesh::All,
        ..Default::default()
    };
    // Decode the journal using its most recently recorded windows while the device opens.
    // If the driver chose a different layout, replay the same file with the actual window byte.
    let (real, mut journal_time) = std::thread::scope(|scope| -> Result<_, String> {
        let read = file.as_mut().map(|file| {
            std::thread::Builder::new().name("xcl-journal-load".into()).spawn_scoped(scope, || {
                let start = std::time::Instant::now();
                q.read_snapshot(file)?;
                Ok::<_, std::io::Error>(start.elapsed())
            })
        }).transpose().map_err(|e| format!("Starting journal reader: {e}"))?;
        let real = matches!(crate::hle::d3d12x::backend(), crate::hle::d3d12x::Backend::Real);
        let shaders = prepare_shaders();
        let read = read.map(|reader| reader.join());
        shaders?;
        let elapsed = match read {
            Some(joined) if real => joined.map_err(|_| "Pipeline journal reader panicked".to_string())?
                .map_err(|e| format!("Reading {}: {e}", path.display()))?,
            _ => std::time::Duration::ZERO,
        };
        Ok((real, elapsed))
    })?;
    if !real { return Ok(()); }
    let actual = pso_journal::windows_byte(crate::gpu::recomp::pso::mem_windows());
    if q.windows != actual {
        let start = std::time::Instant::now();
        q.select_snapshot_windows(actual, file.as_mut()).map_err(|e| format!("Reading {}: {e}", path.display()))?;
        journal_time += start.elapsed();
    }
    let seed_start = std::time::Instant::now();
    let captured = q.pipes.len();
    q.seed_compute(std::mem::take(&mut *progress.compute_sources.lock().unwrap()));
    let seeded = q.pipes.len() - captured;
    crate::xlog_info!("PSO preparation: journal read and decoded in {:.2} s, compute recipes added in {:.2} s",
        journal_time.as_secs_f64(), seed_start.elapsed().as_secs_f64());
    let total: usize = q.pipes.keys().map(|h| q.copies(*h)).sum();
    progress.total.fetch_add(total, Ordering::Relaxed);
    let prior = progress.done.load(Ordering::Relaxed);
    let _ = progress.pipeline_phase.set(prior);
    let processed = || {
        let done = progress.done.fetch_add(1, Ordering::Relaxed) + 1 - prior;
        if done % 1000 == 0 {
            crate::xlog_info!("PSO preparation: {done} of {total} pipeline objects processed");
        }
    };
    crate::xlog_info!("PSO preparation: {captured} captured recipes, {seeded} pack-derived compute recipes, {total} objects, {} demanded, {} previously built, {} other-window entries", q.demanded.len(), q.used.len(), q.other_windows);
    use windows::Win32::Graphics::Direct3D12::ID3D12Device;
    use windows::core::Interface;
    let raw = crate::hle::d3d12x::real::device_raw();
    let index = unsafe { ID3D12Device::from_raw_borrowed(&raw) }.and_then(native_key).and_then(|key| {
        if std::env::var("XCL_PSO_DISK_CACHE").is_ok_and(|v| v.trim() == "0") { return None; }
        warm_replay::Index::open(path.parent()?, progress.driver_files()?.as_ref(), key, q.windows)
    });
    let all_live = std::env::var("XCL_PREPARE_ALL_LIVE").is_ok_and(|v| v.trim() == "1");
    let mut hot = q.was_used.clone();
    hot.extend(q.demanded.iter().copied());
    let disk_ready = AtomicUsize::new(0);
    let queue = Mutex::new(q);
    let ready = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);
    let workers = std::env::var("XCL_PIPELINE_THREADS").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(4).max(1).min(total);
    std::thread::scope(|scope| -> Result<(), String> {
        let mut jobs = Vec::new();
        for n in 0..workers {
            let job = std::thread::Builder::new().name(format!("xcl-pso-prepare-{n}")).stack_size(WARM_STACK).spawn_scoped(scope, || {
                while !progress.cancel.load(Ordering::Relaxed) {
                    let (hash, recipe, copies) = {
                        let mut q = queue.lock().unwrap();
                        let Some((hash, recipe)) = q.next() else { break };
                        (hash, recipe, q.copies(hash))
                    };
                    if !all_live && copies == 1 && recipe.is_some() && !matches!(recipe, Some(Replay::Mesh(..))) && !hot.contains(&hash)
                        && index.as_ref().is_some_and(|book| book.can_defer(hash)) {
                        disk_ready.fetch_add(1, Ordering::Relaxed);
                        processed();
                        continue;
                    }
                    for _ in 0..copies {
                        if progress.cancel.load(Ordering::Relaxed) { break; }
                        match &recipe {
                            Some(Replay::Graphics(d, ..)) if d.HS.BytecodeLength != 0 || d.DS.BytecodeLength != 0 => {
                                skipped.fetch_add(1, Ordering::Relaxed);
                            }
                            Some(r) if {
                                let build = || unsafe { crate::hle::d3d12x::real::prepare_live(hash, r) };
                                if let Some(book) = &index {
                                    let (ok, files) = warm_replay::capture(build);
                                    if ok { book.add(hash, files); }
                                    ok
                                } else { build() }
                            } => {
                                ready.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {
                                let n = failed.fetch_add(1, Ordering::Relaxed);
                                if n < 10 {
                                    let kind = match &recipe {
                                        Some(Replay::Mesh(..)) => "mesh",
                                        Some(Replay::Compute(..)) => "compute",
                                        Some(Replay::Graphics(..)) => "graphics",
                                        None => "undecodable journal recipe",
                                    };
                                    let verified = index.as_ref().is_some_and(|book| book.can_defer(hash));
                                    crate::xlog_warn!("PSO preparation: {kind} recipe {hash:016x} failed (verified on disk: {verified}, hot: {}, copies: {copies})", hot.contains(&hash));
                                }
                            }
                        }
                        processed();
                    }
                }
            }).map_err(|e| format!("Starting pipeline preparation worker: {e}"))?;
            jobs.push(job);
        }
        for job in jobs { job.join().map_err(|_| "Pipeline preparation worker panicked".to_string())?; }
        Ok(())
    })?;
    let (ready, failed, skipped) = (ready.into_inner(), failed.into_inner(), skipped.into_inner());
    crate::xlog_info!("PSO preparation: {ready} live pipelines ready, {failed} failed, {skipped} unsupported before guest entry");
    if progress.cancel.load(Ordering::Relaxed) { return Err("Pipeline preparation cancelled".into()); }
    if failed != 0 { return Err(format!("{failed} captured pipeline recipes failed before guest entry")); }
    crate::xlog_info!("PSO preparation: {} verified disk recipes left for first use", disk_ready.into_inner());
    if let Some(book) = index { book.save(); }
    Ok(())
}

impl Queue {
    fn requeue_unverified(&mut self, done: &[u64], protected: &HashSet<u64>, verified: impl Fn(u64) -> bool) -> usize {
        let mut revisit = 0;
        for &hash in done {
            if protected.contains(&hash) || verified(hash) || !self.taken.remove(&hash) {
                continue;
            }
            if self.pipes.contains_key(&hash) {
                if self.was_used.contains(&hash) { self.used.push_back(hash); }
                else { self.rest.push_back(hash); }
                revisit += 1;
            }
        }
        revisit
    }

    fn select_snapshot_windows(&mut self, actual: u8, file: Option<&mut (impl Read + Seek)>) -> std::io::Result<()> {
        *self = Self { windows: actual, prepare_numbered_mesh: self.prepare_numbered_mesh, ..Default::default() };
        if let Some(file) = file {
            file.rewind()?;
            self.read_snapshot(file)?;
        }
        Ok(())
    }

    fn read_snapshot(&mut self, reader: &mut impl Read) -> std::io::Result<()> {
        reader.read_to_end(&mut self.buf)?;
        self.ingest_frames();
        if self.parsed == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "unrecognized pipeline journal"));
        }
        if self.parsed < self.buf.len() {
            crate::xlog_warn!("PSO preparation: ignored {} bytes of an incomplete final journal frame", self.buf.len() - self.parsed);
        }
        // Replay owns its shader sources and descriptors. Only the watcher needs
        // the raw bytes to pick up the next append.
        self.buf = Vec::new();
        Ok(())
    }

    fn seed_compute(&mut self, sources: HashMap<u64, Vec<u8>>) {
        for (source, bytes) in sources {
            if crate::gpu::shader::fnv64(&bytes) != source { continue; }
            self.shaders.entry(source).or_insert_with(|| Arc::new(bytes));
            // Both forms select node zero on a single-node device. Guest objects still
            // have distinct identities. Keep the queue position of any captured form.
            for node in [0, 1] {
                let desc = windows::Win32::Graphics::Direct3D12::D3D12_COMPUTE_PIPELINE_STATE_DESC { NodeMask: node, ..Default::default() };
                let (hash, payload) = pso_journal::compute_recipe(source, &desc, self.windows);
                if let std::collections::hash_map::Entry::Vacant(slot) = self.pipes.entry(hash) {
                    slot.insert((b'C', Arc::new(payload)));
                    self.rest.push_back(hash);
                }
            }
        }
    }

    // Count distinct observed ordinal frames. A corrupt length or ordinal cannot cause
    // unbounded allocations, and rereading another run cannot inflate the count.
    fn copies(&self, hash: u64) -> usize {
        1 + self.repeats.get(&hash).map_or(0, HashSet::len)
    }

    /// Read what was added to the journal; the new work.
    fn ingest(&mut self, file: &mut std::fs::File) -> usize {
        // a torn last frame may be cut and written over by the next run: read it again
        self.buf.truncate(self.parsed);
        // a journal made again (shorter than what was read): from its start
        if file.metadata().is_ok_and(|m| m.len() < self.buf.len() as u64) {
            self.buf.clear();
            self.parsed = 0;
        }
        let start = self.buf.len();
        let _ = file.seek(std::io::SeekFrom::Start(start as u64));
        let _ = file.read_to_end(&mut self.buf);
        if self.buf.len() == start && self.parsed != 0 {
            return 0;
        }
        self.ingest_frames()
    }

    fn ingest_frames(&mut self) -> usize {
        if self.parsed == 0 {
            if !self.buf.starts_with(pso_journal::MAGIC) {
                return 0;
            }
            self.parsed = pso_journal::MAGIC.len();
        }
        let mut new = 0;
        let mut frames = Vec::new();
        self.parsed = pso_journal::for_each_frame_from(&self.buf, self.parsed, |f| frames.push((f.tag, f.windows, f.hash, f.payload.to_vec())));
        if self.windows == 0 {
            // the last run's (a run writes all its pipelines again when its windows differ)
            self.windows = frames.iter().rev().find(|f| matches!(f.0, b'G' | b'C' | b'M')).map_or(0, |f| f.1.max(1));
        }
        for (tag, windows, hash, payload) in frames {
            let (tag, hash, payload) = if tag == b'X' && self.prepare_numbered_mesh && windows.max(1) == self.windows {
                let projected = pso_journal::captured_numbered_mesh(payload, windows, hash, |source| {
                    *self.mesh_sources.entry(source).or_insert_with(||
                        self.shaders.get(&source).and_then(|blob| crate::gpu::xbox::parse(blob))
                            .is_some_and(|shader| shader.stage == "MS"))
                });
                let Some((key, wire)) = projected else { continue };
                (b'M', key, wire)
            } else { (tag, hash, payload) };
            match tag {
                b'S' => {
                    self.mesh_sources.remove(&hash);
                    if crate::gpu::shader::fnv64(&payload) == hash {
                        self.shaders.insert(hash, Arc::new(payload));
                    } else {
                        self.shaders.remove(&hash);
                    }
                }
                b'G' | b'C' | b'M' => {
                    if windows.max(1) != self.windows {
                        self.other_windows += 1;
                        continue;
                    }
                    self.pipes.insert(hash, (tag, Arc::new(payload)));
                    if !self.taken.contains(&hash) {
                        if self.was_used.contains(&hash) {
                            self.used.push_back(hash);
                        } else {
                            self.rest.push_back(hash);
                        }
                        new += 1;
                    }
                }
                b'R' if windows == self.windows && payload.len() == 12 && crate::gpu::shader::fnv64(&payload) == hash => {
                    let recipe = u64::from_le_bytes(payload[..8].try_into().unwrap());
                    let ordinal = u32::from_le_bytes(payload[8..].try_into().unwrap());
                    if ordinal > 1 { self.repeats.entry(recipe).or_default().insert(ordinal); }
                }
                b'N' => self.demanded.push_back(hash),
                b'U' => {
                    if self.was_used.insert(hash) && self.pipes.contains_key(&hash) && !self.taken.contains(&hash) {
                        self.used.push_back(hash);
                    }
                }
                _ => {}
            }
        }
        new
    }

    fn next(&mut self) -> Option<(u64, Option<Replay>)> {
        while let Some(h) = self.demanded.pop_front().or_else(|| self.used.pop_front()).or_else(|| self.rest.pop_front()) {
            let Some((tag, payload)) = self.pipes.get(&h) else { continue };
            if !self.taken.insert(h) { continue; }
            let (tag, payload) = (*tag, payload.clone());
            let frame = pso_journal::Frame { tag, windows: self.windows, hash: h, payload: &payload };
            return Some((h, pso_journal::replay_verified(&frame, |s| self.shaders.get(&s).cloned())));
        }
        None
    }

    fn left(&self) -> usize {
        self.pipes.keys().filter(|h| !self.taken.contains(h)).count()
    }
}

pub fn run(target: &Path, args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return 0;
    }
    let opt = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<usize>().ok());
    let watch = args.iter().any(|a| a == "--watch");
    let limit = opt("--limit").unwrap_or(usize::MAX);
    let journal = match journal_of(target) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("xcl pipelines: {e}");
            return 2;
        }
    };
    let (dev, gpu_key) = match device(&adapter_setting(&journal)) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("xcl pipelines: no GPU device: {e}");
            return 2;
        }
    };
    #[cfg(test)]
    if std::env::var_os("XCL_SAMPLE_COLD_GRAPHICS_JOURNAL").is_some() {
        use windows::core::Interface;
        use windows::Win32::Graphics::Direct3D12::*;
        let implicit = D3D12_SHADER_CACHE_KIND_FLAG_IMPLICIT_DRIVER_MANAGED | D3D12_SHADER_CACHE_KIND_FLAG_IMPLICIT_D3D_CACHE_FOR_DRIVER;
        let dev9 = dev.cast::<ID3D12Device9>().expect("cold measurement needs ID3D12Device9");
        unsafe { dev9.ShaderCacheControl(implicit, D3D12_SHADER_CACHE_CONTROL_FLAG_DISABLE) }
            .expect("cold measurement requires implicit cache control");
        println!("Cold replay: disabled implicit D3D and driver shader caches");
    }
    // a background job: the title's threads and the user's programs first, and about half the
    // cores (DXC's pool too, unless XCL_DXC_THREADS says otherwise) – the PC stays usable
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS};
        let _ = SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
    }
    let half = std::thread::available_parallelism().map_or(2, |n| n.get() / 2).max(1);
    if std::env::var_os("XCL_DXC_THREADS").is_none() {
        std::env::set_var("XCL_DXC_THREADS", half.to_string());
    }
    // `wait_for_memory` keeps 6 GB of commit free, as next to a running title: the tool runs in
    // the background of a PC in use, and a title's last pipelines take GBs each in DXC
    crate::gpu::recomp::dxc::set_memory_reserve(6);
    crate::hle::d3d12x::real::prepare_device(dev.clone());
    let gpu_most = std::env::var("XCL_PIPELINE_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(if watch { 2 } else { 4usize }).max(1);
    let threads = opt("--threads").unwrap_or(half).max(1);

    let mut file = match std::fs::File::open(&journal) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("xcl pipelines: {}: {e}", journal.display());
            return 2;
        }
    };
    let q = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
    // what an earlier run with this preparation layout finished
    let done_path = journal.with_extension("done");
    let key = offline_key(gpu_key);
    println!("Translation cache {:016x} at {}", crate::gpu::recomp::pso::translation_cache_identity(),
        crate::gpu::recomp::dxc::cache_dir().display());
    let (mut done_file, done) = match open_done(&done_path, key) {
        Ok(opened) => opened,
        Err(e) => {
            eprintln!("xcl pipelines: {}: {e}", done_path.display());
            return 2;
        }
    };
    let mut already = done.len();
    q.0.lock().unwrap().taken.extend(done.iter().copied());
    let deferred_path = journal.with_extension("deferred");
    let (mut deferred_file, deferred) = match open_done(&deferred_path, key) {
        Ok(opened) => opened,
        Err(e) => { eprintln!("xcl pipelines: {}: {e}", deferred_path.display()); return 2; }
    };
    if !deferred.is_empty() { eprintln!("xcl pipelines: {} recipes deferred by earlier runs", deferred.len()); }
    q.0.lock().unwrap().taken.extend(deferred.iter().copied());
    // what a run that crashed (in the driver or DXC) was making: left to the title, so the same
    // pipeline does not end every run (a run the user stopped removes the file)
    let trying_path = journal.with_extension("trying");
    let crashed: Vec<u64> = {
        let t = std::fs::read(&trying_path).unwrap_or_default();
        let mut g = q.0.lock().unwrap();
        t.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).filter(|h| g.taken.insert(*h)).collect()
    };
    for h in &crashed {
        if let Err(e) = deferred_file.write_all(&h.to_le_bytes()) {
            eprintln!("xcl pipelines: {}: {e}", deferred_path.display());
            return 2;
        }
    }
    if !crashed.is_empty() {
        eprintln!("xcl pipelines: the last run ended while making {} pipelines: left to the title", crashed.len());
    }
    let trying = match std::fs::File::create(&trying_path) {
        Ok(f) => {
            let _ = TRYING_PATH.set(trying_path.clone());
            unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_console_close), 1) };
            Mutex::new(Trying { file: f, now: Vec::new() })
        }
        Err(e) => {
            eprintln!("xcl pipelines: {}: {e}", trying_path.display());
            return 2;
        }
    };
    let mut first = {
        let mut g = q.0.lock().unwrap();
        let first = if watch {
            g.ingest(&mut file)
        } else {
            if let Err(e) = g.read_snapshot(&mut file) {
                eprintln!("xcl pipelines: {}: {e}", journal.display());
                return 2;
            }
            g.left()
        };
        // the HLSL (and so the cache keys) of the run's memory windows
        crate::gpu::recomp::pso::set_mem_windows(pso_journal::mem_windows_of(g.windows));
        first
    };
    let t0 = std::time::Instant::now();
    let windows = q.0.lock().unwrap().windows;
    let index = (windows != 0).then(|| native_key(&dev)).flatten().and_then(|key| {
        let driver = crate::gpu::recomp::dxc::cache_dir().join("driver");
        std::fs::create_dir_all(&driver).ok()?;
        let files = warm_replay::driver_files(&driver)?;
        warm_replay::Index::open(journal.parent()?, &files, key, windows)
    });
    // A `.done` record may have been written just before an interrupted run lost its
    // batched index update. Revisit only recipes without a still-valid driver blob.
    if !watch || windows != 0 {
        let mut g = q.0.lock().unwrap();
        let protected: HashSet<u64> = deferred.iter().chain(&crashed).copied().collect();
        let revisit = g.requeue_unverified(&done, &protected, |hash| index.as_ref().is_some_and(|book| book.can_defer(hash)));
        first = g.left();
        already = already.saturating_sub(revisit);
    }
    {
        let g = q.0.lock().unwrap();
        println!(
            "{}: {first} pipelines to make ({} the title used), {already} done before, {} shaders; {} memory windows{}; {threads} threads, {gpu_most} driver compiles at once{}",
            journal.display(),
            g.used.len(),
            g.shaders.len(),
            g.windows & 0x7F,
            if g.windows & 0x80 != 0 { " (from the descriptor heap)" } else { "" },
            if watch { "; following the journal (Ctrl+C stops)" } else { "" }
        );
    }

    let (made, failed, skipped) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0));
    let reasons: Mutex<HashMap<String, usize>> = Mutex::new(HashMap::new());
    let done_tx = Mutex::new(&mut done_file);
    let deferred_tx = Mutex::new(&mut deferred_file);
    let gpu = (Mutex::new(0usize), Condvar::new());
    let started = AtomicUsize::new(0);
    std::thread::scope(|s| {
        if watch {
            let q = q.clone();
            s.spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let mut g = q.0.lock().unwrap();
                if g.closed {
                    return;
                }
                let new = g.ingest(&mut file);
                // (a journal that was empty when the tool started)
                crate::gpu::recomp::pso::set_mem_windows(pso_journal::mem_windows_of(g.windows));
                if new > 0 || !g.used.is_empty() {
                    q.1.notify_all();
                }
            });
        }
        let mut workers = Vec::new();
        for k in 0..threads {
            let (q, dev, made, failed, skipped, reasons, done_tx, deferred_tx, gpu, started, trying, index) = (&q, &dev, &made, &failed, &skipped, &reasons, &done_tx, &deferred_tx, &gpu, &started, &trying, &index);
            let w = std::thread::Builder::new().name(format!("xcl-pso-offline-{k}")).stack_size(WARM_STACK).spawn_scoped(s, move || loop {
                let item = {
                    let mut g = q.0.lock().unwrap();
                    loop {
                        if started.load(Ordering::Relaxed) >= limit {
                            g.closed = true;
                            q.1.notify_all();
                            return;
                        }
                        if let Some(it) = g.next() {
                            started.fetch_add(1, Ordering::Relaxed);
                            break it;
                        }
                        if !watch || g.closed {
                            return;
                        }
                        g = q.1.wait(g).unwrap();
                    }
                };
                let (hash, replay) = item;
                trying.lock().unwrap().set(hash, true);
                let mut r = None;
                let (ok, files) = warm_replay::capture(|| {
                    let result = match &replay {
                        None => Err("not readable from the journal".to_string()),
                        // the driver's compiles, few at once (the biggest take ~1.5 GB each)
                        Some(r) => unsafe { crate::hle::d3d12x::real::warm(dev, r, || DriverSlot::take(gpu, gpu_most)) },
                    };
                    let ok = result.is_ok();
                    r = Some(result);
                    ok
                });
                if ok {
                    if let Some(book) = index { book.add(hash, files); }
                }
                let r = r.unwrap();
                // Only successfully built recipes earn completion receipts. Driver crashes
                // and unsupported forms remain distinguishable when the pass is resumed.
                let receipt = match r {
                    Ok(()) => {
                        made.fetch_add(1, Ordering::Relaxed);
                        Some(done_tx)
                    }
                    Err(e) if e.contains("left to the title") => {
                        skipped.fetch_add(1, Ordering::Relaxed);
                        Some(deferred_tx)
                    }
                    Err(e) => {
                        let reason: String = e.lines().find(|l| l.contains("error")).unwrap_or(e.lines().next().unwrap_or("")).chars().take(160).collect();
                        *reasons.lock().unwrap().entry(reason).or_default() += 1;
                        failed.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                };
                if let Some(file) = receipt {
                    if let Err(e) = file.lock().unwrap().write_all(&hash.to_le_bytes()) {
                        *reasons.lock().unwrap().entry(format!("could not save checkpoint: {e}")).or_default() += 1;
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
                trying.lock().unwrap().set(hash, false);
            });
            match w {
                Ok(w) => workers.push(w),
                Err(e) => {
                    eprintln!("xcl pipelines: could not start worker {k}: {e}");
                    failed.fetch_add(1, Ordering::Relaxed);
                    let mut g = q.0.lock().unwrap();
                    if workers.is_empty() { g.closed = true; }
                    q.1.notify_all();
                    break;
                }
            }
        }
        // progress every 10 s, also while the driver takes long over the biggest programs
        let (mut last, mut said) = (0, false);
        while !workers.iter().all(|w| w.is_finished()) {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if t0.elapsed().as_secs() < last + 10 {
                continue;
            }
            last = t0.elapsed().as_secs();
            let n = made.load(Ordering::Relaxed) + failed.load(Ordering::Relaxed) + skipped.load(Ordering::Relaxed);
            let (left, busy) = (q.0.lock().unwrap().left(), trying.lock().unwrap().now.len());
            let rate = n as f64 / t0.elapsed().as_secs_f64().max(0.001);
            eprintln!("  {n} done, {busy} being made, {left} left ({rate:.1}/s)");
            if rate < 1.0 && busy > 0 && !said {
                said = true;
                eprintln!("  (the biggest compute programs take 30 s or more each in the driver; stopping is safe, a re-run goes on from here)");
            }
        }
    });
    // ended by itself: nothing was cut short
    drop(trying);
    let _ = std::fs::remove_file(&trying_path);
    let g = q.0.lock().unwrap();
    let failed = failed.into_inner();
    if let Some(book) = index { book.save(); }
    println!(
        "made {}, failed {}, deferred to live preparation {}, time {:.1} s",
        made.into_inner(),
        failed,
        skipped.into_inner(),
        t0.elapsed().as_secs_f64()
    );
    if g.other_windows > 0 {
        println!("  {} pipelines from a run with other memory windows left out", g.other_windows);
    }
    let mut r: Vec<_> = reasons.into_inner().unwrap().into_iter().collect();
    r.sort_by(|a, b| b.1.cmp(&a.1));
    for (why, n) in r.iter().take(10) {
        println!("  {n}× {why}");
    }
    if failed == 0 { 0 } else { 2 }
}

/// A driver compile's turn (`XCL_PIPELINE_THREADS` at once), given back when dropped.
struct DriverSlot<'a>(&'a (Mutex<usize>, Condvar));

impl<'a> DriverSlot<'a> {
    fn take(gpu: &'a (Mutex<usize>, Condvar), most: usize) -> DriverSlot<'a> {
        let mut n = gpu.0.lock().unwrap();
        while *n >= most {
            n = gpu.1.wait(n).unwrap();
        }
        *n += 1;
        DriverSlot(gpu)
    }
}

impl Drop for DriverSlot<'_> {
    fn drop(&mut self) {
        *self.0 .0.lock().unwrap() -= 1;
        self.0 .1.notify_one();
    }
}

/// The GPU the title's runs use: its `xcl.toml`'s `d3d12_adapter` (or `XCL_ADAPTER`), as
/// `real::init` picks it – the driver's cache and the wave width are the adapter's.
fn adapter_setting(journal: &Path) -> String {
    let toml = journal.parent().and_then(Path::parent).map(|d| d.join("xcl.toml"));
    match toml.filter(|t| t.is_file()).map(|t| crate::config::Config::load_or_create(&t)) {
        Some(Ok(c)) => c.d3d12_adapter,
        _ => "auto".into(),
    }
}

/// The device, and its adapter's vendor, device and driver version (for `offline_key`).
fn device(adapter: &str) -> Result<(windows::Win32::Graphics::Direct3D12::ID3D12Device, u64), String> {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::*;
    unsafe {
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).map_err(|e| e.to_string())?;
        let sel = crate::gpu::adapter::Selector::from_env_or(adapter);
        let (adapter, desc) = crate::gpu::adapter::candidates(&factory, &sel).into_iter().next().ok_or_else(|| crate::gpu::adapter::no_match(&factory, &sel))?;
        let umd = adapter.CheckInterfaceSupport(&<IDXGIDevice as windows::core::Interface>::IID).unwrap_or(0);
        let key = crate::gpu::shader::fnv64(&[(desc.VendorId as u64) << 32 | desc.DeviceId as u64, umd as u64].map(u64::to_le_bytes).concat());
        Ok((crate::gpu::agility::create_device(&adapter, D3D_FEATURE_LEVEL_12_0).map_err(|e| e.to_string())?, key))
    }
}

/// Opens `pipelines.done` for `key` (this preparation layout): the hashes it holds, and the file
/// positioned after them. A file for another layout starts over; a torn last record is cut off.
fn open_done(path: &Path, key: u64) -> std::io::Result<(std::fs::File, Vec<u64>)> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_SHARE_READ: a second `xcl pipelines` on this journal is refused, not interleaved
    let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(1).open(path)?;
    let mut old = Vec::new();
    f.read_to_end(&mut old)?;
    let done: Vec<u64> = if old.len() >= 8 && old[..8] == key.to_le_bytes() {
        old[8..].chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
    } else {
        Vec::new()
    };
    if done.is_empty() {
        f.set_len(0)?;
        f.seek(std::io::SeekFrom::Start(0))?;
        f.write_all(&key.to_le_bytes())?;
    } else {
        let end = 8 + 8 * done.len() as u64;
        f.set_len(end)?;
        f.seek(std::io::SeekFrom::Start(end))?;
    }
    Ok((f, done))
}

#[cfg(test)]
mod tests {
    #[test]
    fn captured_numbered_mesh_prepares_the_independently_verified_recipes() {
        let Some(input) = std::env::var_os("XCL_SAMPLE_NUMBERED_MESH_CAPTURE") else { return };
        let expected = std::env::var_os("XCL_SAMPLE_NUMBERED_MESH_EXPECTED").expect("supply verified M recipes");
        let mut reference = super::Queue::default();
        reference.read_snapshot(&mut std::fs::File::open(expected).unwrap()).unwrap();
        assert!(!reference.pipes.is_empty());
        let mut capture = super::Queue {
            windows: reference.windows, prepare_numbered_mesh: true, ..Default::default()
        };
        capture.read_snapshot(&mut std::fs::File::open(input).unwrap()).unwrap();
        for (key, wire) in &reference.pipes {
            assert_eq!(wire.0, b'M');
            assert_eq!(capture.pipes.get(key), Some(wire));
            assert_eq!(capture.copies(*key), reference.copies(*key));
        }
        let objects: usize = reference.pipes.keys().map(|k| capture.copies(*k)).sum();
        println!("Recovered {} verified mesh recipes from original guest-slot captures ({objects} objects)", reference.pipes.len());
    }
    use super::*;

    #[test]
    fn cold_graphics_replay_with_pretranslated_fixture() {
        let Some(path) = std::env::var_os("XCL_SAMPLE_COLD_GRAPHICS_JOURNAL") else { return };
        let journal = Path::new(&path);
        let cache = crate::gpu::recomp::dxc::cache_dir();
        let translation = crate::gpu::recomp::pso::translation_cache_identity();
        let namespace = cache.join("translated").join("v2").join(format!("{translation:016x}"));
        assert!(namespace.is_dir() && !cache.join("driver").exists(), "need compatible pretranslated shaders and an empty driver cache");
        assert!(!journal.with_extension("done").exists() && !journal.with_extension("trying").exists());
        let before = crate::gpu::recomp::dxc::cache_stats().compilations;
        assert_eq!(run(journal, &["--threads".into(), "4".into()]), 0);
        assert_eq!(crate::gpu::recomp::dxc::cache_stats().compilations, before, "measurement must exclude DXC");
    }

    #[test]
    fn replay_warms_split_pieces_and_both_whole_pipeline_signatures() {
        use crate::gpu::recomp::{dxc, pso};
        use crate::hle::d3d12x::real::{driver_cache, warm};
        use windows::Win32::Graphics::Direct3D12::*;
        let Some(sample) = std::env::var_os("XCL_SAMPLE_OFFLINE_CHAIN") else { return };
        let cache = dxc::cache_dir();
        assert!(!cache.exists(), "use an empty private XCL_DXIL_CACHE");
        let Ok((dev, _)) = device("auto") else { return crate::gpu::recomp::runner::skip_test("no D3D12 adapter"); };
        let code = std::fs::read(sample).unwrap();
        let compiled = pso::console_to_dxil_for(&code, pso::Stage::Compute, 0, pso::wave32_only_for_device(&dev)).unwrap();
        let chain = compiled.dispatch_chain.as_ref().expect("sample must split");
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
            ..Default::default()
        };
        let recipe = pso_journal::Replay::Compute(Box::new(desc), vec![Arc::new(code)]);
        crate::hle::d3d12x::real::prepare_device(dev.clone());
        unsafe { warm(&dev, &recipe, || ()).unwrap(); }
        let verify = |dxil: &[u8], chained| {
            let path = driver_cache::path_for(dxil, 0, chained).unwrap();
            assert!(path.starts_with(&cache) && driver_cache::load(&path).is_some(), "missing {}", path.display());
        };
        verify(&compiled.dxil, false);
        verify(&compiled.dxil, true);
        for piece in &chain.pieces { verify(piece, true); }
    }

    #[test]
    fn startup_keeps_only_the_render_devices_window_layout() {
        let mut bytes = pso_journal::MAGIC.to_vec();
        bytes.extend(frame(b'C', 41, &[0; 8]));
        let mut latest = frame(b'C', 82, &[0; 8]);
        latest[1] = 0x84;
        bytes.extend(latest);
        let mut file = std::io::Cursor::new(bytes);
        let mut q = Queue { prepare_numbered_mesh: true, ..Default::default() };
        q.read_snapshot(&mut file).unwrap();
        assert_eq!(q.windows, 0x84);
        assert_eq!(q.rest, VecDeque::from([82]));
        q.select_snapshot_windows(4, Some(&mut file)).unwrap();
        assert_eq!(q.windows, 4);
        assert!(q.prepare_numbered_mesh);
        assert_eq!(q.rest, VecDeque::from([41]));
        assert!(!q.pipes.contains_key(&82));
        assert_eq!(q.other_windows, 1);
    }

    #[test]
    fn startup_propagates_read_error_after_a_valid_journal_prefix() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "test read error"))
            }
        }
        let mut reader = pso_journal::MAGIC.as_slice().chain(Broken);
        let mut q = Queue::default();
        assert_eq!(q.read_snapshot(&mut reader).unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);
        assert_eq!(q.parsed, 0);
    }

    #[test]
    fn pack_compute_without_journal_replays_both_node_masks_once() {
        let bytes = vec![1, 2, 3, 4];
        let source = crate::gpu::shader::fnv64(&bytes);
        let mut q = Queue { windows: 0x84, ..Default::default() };
        for _ in 0..2 { q.seed_compute(HashMap::from([(source, bytes.clone())])); }
        for node in [0, 1] {
            let (_, Some(Replay::Compute(desc, blobs))) = q.next().unwrap() else { panic!("missing compute recipe") };
            assert!(Arc::ptr_eq(&blobs[0], &q.shaders[&source]));
            assert_eq!(desc.NodeMask, node);
            assert_eq!(desc.Flags.0, 0);
            assert_eq!(*blobs[0], bytes);
        }
        assert!(q.next().is_none());
    }

    #[test]
    fn pack_compute_fills_missing_mask_after_captured_requests() {
        use windows::Win32::Graphics::Direct3D12::D3D12_COMPUTE_PIPELINE_STATE_DESC;
        let bytes = vec![10, 11, 12];
        let source = crate::gpu::shader::fnv64(&bytes);
        for windows in [1, 0x84] {
            for captured_node in [0, 1] {
                let mut q = Queue { windows, ..Default::default() };
                let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC { NodeMask: captured_node, ..Default::default() };
                let (captured, payload) = pso_journal::compute_recipe(source, &desc, windows);
                q.pipes.insert(captured, (b'C', Arc::new(payload)));
                q.demanded.push_back(captured);
                q.repeats.entry(captured).or_default().insert(2);
                q.seed_compute(HashMap::from([(source, bytes.clone())]));
                assert_eq!(q.pipes.len(), 2);
                assert_eq!(q.rest.len(), 1);
                for node in [captured_node, 1 - captured_node] {
                    let (hash, Some(Replay::Compute(d, blobs))) = q.next().unwrap() else { panic!("missing compute recipe") };
                    assert_eq!(d.NodeMask, node);
                    assert_eq!(*blobs[0], bytes);
                    assert_eq!(hash, pso_journal::compute_recipe(source, &d, windows).0);
                    assert_eq!(q.copies(hash), if node == captured_node { 2 } else { 1 });
                }
                assert!(q.next().is_none());
            }
        }
    }

    #[test]
    fn a_torn_frame_written_over_is_read_again() {
        let path = std::env::temp_dir().join(format!("xcl_pipelines_torn_{}.bin", std::process::id()));
        let mut j = pso_journal::MAGIC.to_vec();
        j.extend(frame(b'C', 1, &[0; 8]));
        let good = j.len();
        j.extend(&frame(b'C', 2, &[0; 8])[..9]); // a run killed mid-frame
        std::fs::write(&path, &j).unwrap();
        let mut f = std::fs::File::open(&path).unwrap();
        let mut q = Queue::default();
        assert_eq!(q.ingest(&mut f), 1);
        // the next run cuts the torn frame and writes on from there
        j.truncate(good);
        j.extend(frame(b'C', 3, &[0; 8]));
        j.extend(frame(b'C', 4, &[0; 8]));
        std::fs::write(&path, &j).unwrap();
        assert_eq!(q.ingest(&mut f), 2, "3 and 4, read from where the torn frame began");
        drop(f);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn demand_precedes_background_build_and_is_returned_once() {
        let mut q = Queue::default();
        for h in [1, 2, 3] {
            q.pipes.insert(h, (b'C', Arc::new(vec![])));
            q.rest.push_back(h);
        }
        q.demanded.extend([4, 3, 3]);
        q.used.push_back(2);
        let order: Vec<_> = std::iter::from_fn(|| q.next().map(|(h, _)| h)).collect();
        assert_eq!(order, [3, 2, 1]);
    }

    #[test]
    fn trying_holds_only_the_pipelines_being_made() {
        let path = std::env::temp_dir().join(format!("xcl-trying-test-{}.trying", std::process::id()));
        let mut t = Trying { file: std::fs::File::create(&path).unwrap(), now: Vec::new() };
        t.set(1, true);
        t.set(2, true);
        t.set(3, true);
        t.set(1, false); // made
        t.set(3, false); // failed: tried again by the next run, not left to the title
        drop(t);
        assert_eq!(std::fs::read(&path).unwrap(), 2u64.to_le_bytes());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn another_gpu_or_driver_starts_the_done_file_over() {
        assert_eq!(offline_key(5), offline_key(5));
        assert_ne!(offline_key(5), offline_key(6));
    }

    #[test]
    fn cached_receipts_follow_the_destination_and_translation_identity() {
        let key = |dir: &str, id, split, disk| checkpoint_key(10, 20, 30, Path::new(dir), id, split, disk);
        let original = key("first", 42, false, true);
        for other in [key("other", 42, false, true), key("first", 43, false, true),
            key("first", 42, true, true), key("first", 42, false, false)] {
            assert_ne!(original, other);
        }
    }

    #[test]
    fn done_file_of_another_layout_is_replaced_and_a_torn_record_cut() {
        let path = std::env::temp_dir().join(format!("xcl-done-test-{}.done", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (mut f, done) = open_done(&path, 1).unwrap();
        assert!(done.is_empty());
        f.write_all(&7u64.to_le_bytes()).unwrap();
        f.write_all(&[9, 9, 9]).unwrap(); // a record cut short
        drop(f);
        let (mut f, done) = open_done(&path, 1).unwrap();
        assert_eq!(done, [7]);
        f.write_all(&8u64.to_le_bytes()).unwrap();
        drop(f);
        assert_eq!(std::fs::read(&path).unwrap().len(), 24, "the torn record was cut, 8 follows 7");
        let (mut f, done) = open_done(&path, 2).unwrap();
        assert!(done.is_empty(), "another layout's progress is not reused");
        f.write_all(&5u64.to_le_bytes()).unwrap();
        drop(f);
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(bytes, [2u64.to_le_bytes(), 5u64.to_le_bytes()].concat(), "the old file was replaced, not appended to");
    }

    fn frame(tag: u8, hash: u64, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![tag, 4, 0, 0];
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.extend_from_slice(&hash.to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn malformed_replacement_cannot_replay_using_an_older_verified_shader() {
        let mut q = Queue { buf: pso_journal::MAGIC.to_vec(), ..Default::default() };
        let code = [7, 8, 9];
        let key = crate::gpu::shader::fnv64(&code);
        q.buf.extend(frame(b'S', key, &code));
        q.ingest_frames();
        let live = q.shaders[&key].clone();
        q.buf.extend(frame(b'S', key, &[9, 8, 7]));
        q.ingest_frames();
        assert!(!q.shaders.contains_key(&key));
        assert_eq!(live.as_slice(), code);
        q.buf.extend(frame(b'S', key, &code));
        q.ingest_frames();
        assert_eq!(q.shaders[&key].as_slice(), code);
    }

    #[test]
    fn refused_stream_diagnostics_are_never_scheduled_for_build() {
        let mut q = Queue { buf: pso_journal::MAGIC.to_vec(), ..Default::default() };
        q.buf.extend(frame(b'X', 7, &[0; 8]));
        assert_eq!(q.ingest_frames(), 0);
        assert_eq!(q.windows, 0);
        assert_eq!(q.left(), 0);
        assert!(q.next().is_none());
    }

    #[test]
    fn repeated_requests_keep_separate_objects_without_accumulating_across_runs() {
        let path = std::env::temp_dir().join(format!("xcl_pipelines_repeats_{}.bin", std::process::id()));
        let mut bytes = pso_journal::MAGIC.to_vec();
        let recipe = 7u64;
        bytes.extend(frame(b'C', recipe, &[0; 8]));
        for ordinal in [2u32, 3, 2, 1] {
            let payload = [recipe.to_le_bytes().as_slice(), &ordinal.to_le_bytes()].concat();
            bytes.extend(frame(b'R', crate::gpu::shader::fnv64(&payload), &payload));
        }
        let bad = [recipe.to_le_bytes().as_slice(), &u32::MAX.to_le_bytes()].concat();
        bytes.extend(frame(b'R', 0, &bad));
        std::fs::write(&path, bytes).unwrap();
        let mut q = Queue::default();
        assert_eq!(q.ingest(&mut std::fs::File::open(&path).unwrap()), 1);
        assert_eq!(q.copies(recipe), 3);
        assert_eq!(q.left(), 1, "one durable blob serves all three live objects");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn used_pipelines_first_done_ones_skipped_journal_followed() {
        let path = std::env::temp_dir().join(format!("xcl_pipelines_queue_{}.bin", std::process::id()));
        let mut j = pso_journal::MAGIC.to_vec();
        for h in [1u64, 2, 3] {
            j.extend(frame(b'C', h, &[0; 8]));
        }
        j.extend(frame(b'U', 3, &[]));
        // half a frame: written later
        let tail = frame(b'U', 2, &[]);
        j.extend(&tail[..5]);
        std::fs::write(&path, &j).unwrap();
        let mut f = std::fs::File::open(&path).unwrap();
        let mut q = Queue::default();
        q.taken.insert(1);
        assert_eq!(q.ingest(&mut f), 2, "pipeline 1 was done before");
        assert_eq!(q.windows, 4);
        assert_eq!(q.next().map(|(h, _)| h), Some(3), "the used one first");
        // the rest of the torn frame arrives: 2 is used now
        let mut w = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        w.write_all(&tail[5..]).unwrap();
        drop(w);
        assert_eq!(q.ingest(&mut f), 0);
        assert_eq!(q.next().map(|(h, _)| h), Some(2));
        assert!(q.next().is_none(), "each pipeline once");
        drop(f);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_index_receipt_requeues_a_done_recipe_without_retrying_deferred_work() {
        let mut q = Queue::default();
        for hash in 1..=4 {
            q.pipes.insert(hash, (b'C', Arc::new(vec![0; 8])));
            q.taken.insert(hash);
        }
        q.was_used.insert(2);
        let protected = HashSet::from([3]);
        let revisit = q.requeue_unverified(&[1, 2, 3, 4], &protected, |hash| hash == 1 || hash == 4);
        assert_eq!(revisit, 1);
        assert_eq!(q.used.pop_front(), Some(2));
        assert!(q.rest.is_empty());
        assert!(q.taken.contains(&1) && !q.taken.contains(&2) && q.taken.contains(&3) && q.taken.contains(&4));
    }
}
