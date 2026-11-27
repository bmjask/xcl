//! The pipelines a title asked for, on disk (`<data>\cache\pipelines.bin`), so another process
//! can compile them: `xcl pipelines` reads the journal – during the run (`--watch`) or before
//! the next one – and fills the DXIL cache and the driver's with what the title will ask for.
//! Written for every pipeline with console shaders that goes to `pso_async` (its DXIL was not
//! in the cache, or it is made lazily); a pipeline's first build is marked as used, which the
//! tool compiles first. `XCL_PSO_JOURNAL=0`: off.
//!
//! Append-only frames, little-endian: tag u8, memory windows u8 (`windows_byte`), 0 u16, payload length u32,
//! hash u64, payload. Tags: `S` a shader (its bytes; hash = fnv64 of them), `G` a graphics
//! pipeline, `C` a compute one, `U` (no payload) pipeline `hash` was built. A pipeline's
//! payload names its shaders by hash; each shader is written once, before the first pipeline
//! that uses it. Duplicates of earlier runs are not written again.
//!
//! On the title's pipeline threads an entry is built and hashed with no lock held; the lock
//! covers its append only. The journal is opened (the frame headers of earlier runs read) on a
//! thread of its own, and what is noted meanwhile waits in memory (`State::Opening`).

use std::collections::{HashMap, HashSet};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use windows::Win32::Graphics::Direct3D12::*;

pub const MAGIC: &[u8; 8] = b"XCLPJ001";
pub const HEAD: usize = 16;

pub fn path_in(data_dir: &Path) -> PathBuf {
    data_dir.join("cache").join("pipelines.bin")
}

struct Journal {
    file: std::fs::File,
    seen: HashSet<(u8, u64)>,
}

/// A frame to append: tag, windows byte, hash, payload.
type Out<'a> = (u8, u8, u64, &'a [u8]);

enum State {
    Closed,
    /// being opened (`open_in_background`): the frames noted meanwhile, owned, each once, and
    /// their bytes
    Opening(Vec<(u8, u8, u64, Vec<u8>)>, HashSet<(u8, u64)>, usize),
    Open(Journal),
    Off,
}

static J: Mutex<State> = Mutex::new(State::Closed);
/// set with `State::Off`, so that the title's threads stop building entries
static OFF: AtomicBool = AtomicBool::new(false);
/// what may wait in memory while the journal is opened
const OPENING_MAX: usize = 256 << 20;

pub(super) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("XCL_PSO_JOURNAL").map_or(true, |v| v != "0")) && !OFF.load(Ordering::Relaxed)
}

/// A panic on another thread while it held the lock does not end the journal (each change of
/// the state is one assignment, so it stays whole).
fn lock() -> MutexGuard<'static, State> {
    J.lock().unwrap_or_else(|e| e.into_inner())
}

fn turn_off(g: &mut State, why: impl std::fmt::Display) {
    crate::xlog_warn!("pipeline journal: {why}: off for this run");
    *g = State::Off;
    OFF.store(true, Ordering::Relaxed);
}

fn open() -> Option<Journal> {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::windows::fs::OpenOptionsExt;
    if !enabled() {
        return None;
    }
    let path = path_in(&crate::state::data_dir());
    let _ = std::fs::create_dir_all(path.parent()?);
    // the only writer: another title process gets no journal (`xcl pipelines` only reads)
    const FILE_SHARE_READ: u32 = 1;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(FILE_SHARE_READ)
        .open(&path)
        .map_err(|e| crate::xlog_warn!("pipeline journal {}: {e} – none in this run", path.display()))
        .ok()?;
    let mut seen = HashSet::new();
    let len = file.metadata().map_or(0, |m| m.len());
    let mut magic = [0u8; 8];
    if file.read_exact(&mut magic).is_ok() && &magic == MAGIC {
        // the frames' headers only (the shaders' bytes stay on disk); a torn last frame (a
        // run killed mid-write) is cut off
        let mut at = MAGIC.len() as u64;
        let mut head = [0u8; HEAD];
        while file.read_exact(&mut head).is_ok() {
            let end = at + HEAD as u64 + u32::from_le_bytes(head[4..8].try_into().unwrap()) as u64;
            if end > len {
                break;
            }
            seen.insert((head[0], u64::from_le_bytes(head[8..16].try_into().unwrap())));
            at = end;
            if file.seek(SeekFrom::Start(at)).is_err() {
                break;
            }
        }
        if at < len {
            let _ = file.set_len(at);
        }
    } else {
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = file.write_all(MAGIC);
    }
    file.seek(SeekFrom::End(0)).ok()?;
    crate::xlog_info!("pipeline journal: {} ({} entries from earlier runs) – `xcl pipelines` compiles them outside the title", path.display(), seen.len());
    Some(Journal { file, seen })
}

/// Appends `frames` in order (shaders before the pipeline that names them); a write error turns
/// the journal off for the rest of the run. The first call starts the opening.
fn write(frames: &[Out]) {
    if !enabled() { return; }
    if note(&mut lock(), frames) && std::thread::Builder::new().name("xcl-pso-journal".into()).spawn(open_in_background).is_err() {
        turn_off(&mut lock(), "no thread to open it");
    }
}

/// `write` on the state; true: the caller starts the opening.
fn note(g: &mut State, frames: &[Out]) -> bool {
    match g {
        State::Off => false,
        State::Open(j) => {
            if let Err(e) = frames.iter().try_for_each(|&(t, w, h, p)| put(j, t, w, h, p)) {
                turn_off(g, e);
            }
            false
        }
        State::Opening(q, kept, bytes) => {
            for &(t, w, h, p) in frames {
                if kept.insert((t, h)) {
                    *bytes += p.len();
                    q.push((t, w, h, p.to_vec()));
                }
            }
            if *bytes > OPENING_MAX {
                turn_off(g, "too much noted while it was opened");
            }
            false
        }
        State::Closed => {
            *g = State::Opening(Vec::new(), HashSet::new(), 0);
            note(g, frames);
            true
        }
    }
}

/// Opens the journal with no lock held (the title's threads go on noting), then writes what
/// they noted meanwhile.
fn open_in_background() {
    let j = open();
    opened(&mut lock(), j);
}

fn opened(g: &mut State, j: Option<Journal>) {
    let State::Opening(q, ..) = std::mem::replace(g, State::Off) else { return };
    let Some(mut j) = j else {
        OFF.store(true, Ordering::Relaxed);
        return;
    };
    match q.iter().try_for_each(|(t, w, h, p)| put(&mut j, *t, *w, *h, p)) {
        Ok(()) => *g = State::Open(j),
        Err(e) => turn_off(g, e),
    }
}

fn frame(tag: u8, windows: u8, hash: u64, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEAD + payload.len());
    v.extend_from_slice(&[tag, windows, 0, 0]);
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(&hash.to_le_bytes());
    v.extend_from_slice(payload);
    v
}

/// One frame of the journal.
pub struct Frame<'a> {
    pub tag: u8,
    pub windows: u8,
    pub hash: u64,
    pub payload: &'a [u8],
}

/// Call `f` on each whole frame of `bytes` (a journal from its start); the end of the last whole one.
pub fn for_each_frame<'a>(bytes: &'a [u8], f: impl FnMut(Frame<'a>)) -> usize {
    for_each_frame_from(bytes, MAGIC.len(), f)
}

/// `for_each_frame` from offset `at` (the end of an earlier call: a journal being written).
pub fn for_each_frame_from<'a>(bytes: &'a [u8], mut at: usize, mut f: impl FnMut(Frame<'a>)) -> usize {
    while at + HEAD <= bytes.len() {
        let n = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        if at + HEAD + n > bytes.len() {
            break;
        }
        f(Frame { tag: bytes[at], windows: bytes[at + 1], hash: u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap()), payload: &bytes[at + HEAD..at + HEAD + n] });
        at += HEAD + n;
    }
    at.min(bytes.len())
}

/// Copy each field named into `out` at its offset in `T`: a desc's bytes without its pointers
/// and padding (both stay 0), so the same pipeline always gives the same bytes and hash.
macro_rules! fields {
    ($out:expr, $d:expr, $T:ty, $($($f:ident).+),+ $(,)?) => {{
        $(put_at(&mut $out, std::mem::offset_of!($T, $($f).+), &$d.$($f).+);)+
    }};
}

fn put_at<V>(out: &mut [u8], off: usize, v: &V) {
    let b = unsafe { std::slice::from_raw_parts(v as *const V as *const u8, std::mem::size_of::<V>()) };
    out[off..off + b.len()].copy_from_slice(b);
}

fn graphics_bytes(d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC) -> Vec<u8> {
    type G = D3D12_GRAPHICS_PIPELINE_STATE_DESC;
    type R = D3D12_RENDER_TARGET_BLEND_DESC;
    let mut out = vec![0u8; std::mem::size_of::<G>()];
    fields!(out, d, G, BlendState.AlphaToCoverageEnable, BlendState.IndependentBlendEnable, SampleMask);
    let rt0 = std::mem::offset_of!(G, BlendState.RenderTarget);
    for (i, r) in d.BlendState.RenderTarget.iter().enumerate() {
        let mut one = vec![0u8; std::mem::size_of::<R>()];
        fields!(one, r, R, BlendEnable, LogicOpEnable, SrcBlend, DestBlend, BlendOp, SrcBlendAlpha, DestBlendAlpha, BlendOpAlpha, LogicOp, RenderTargetWriteMask);
        let at = rt0 + i * one.len();
        out[at..at + one.len()].copy_from_slice(&one);
    }
    fields!(
        out, d, G,
        RasterizerState.FillMode, RasterizerState.CullMode, RasterizerState.FrontCounterClockwise, RasterizerState.DepthBias,
        RasterizerState.DepthBiasClamp, RasterizerState.SlopeScaledDepthBias, RasterizerState.DepthClipEnable,
        RasterizerState.MultisampleEnable, RasterizerState.AntialiasedLineEnable, RasterizerState.ForcedSampleCount,
        RasterizerState.ConservativeRaster,
        DepthStencilState.DepthEnable, DepthStencilState.DepthWriteMask, DepthStencilState.DepthFunc, DepthStencilState.StencilEnable,
        DepthStencilState.StencilReadMask, DepthStencilState.StencilWriteMask,
        DepthStencilState.FrontFace.StencilFailOp, DepthStencilState.FrontFace.StencilDepthFailOp,
        DepthStencilState.FrontFace.StencilPassOp, DepthStencilState.FrontFace.StencilFunc,
        DepthStencilState.BackFace.StencilFailOp, DepthStencilState.BackFace.StencilDepthFailOp,
        DepthStencilState.BackFace.StencilPassOp, DepthStencilState.BackFace.StencilFunc,
        InputLayout.NumElements, IBStripCutValue, PrimitiveTopologyType, NumRenderTargets, RTVFormats, DSVFormat,
        SampleDesc.Count, SampleDesc.Quality, NodeMask, Flags,
    );
    out
}

fn compute_bytes(d: &D3D12_COMPUTE_PIPELINE_STATE_DESC) -> Vec<u8> {
    type C = D3D12_COMPUTE_PIPELINE_STATE_DESC;
    let mut out = vec![0u8; std::mem::size_of::<C>()];
    fields!(out, d, C, NodeMask, Flags);
    out
}

unsafe fn bytes_of<'a>(b: &D3D12_SHADER_BYTECODE) -> Option<&'a [u8]> {
    (!b.pShaderBytecode.is_null() && b.BytecodeLength != 0).then(|| std::slice::from_raw_parts(b.pShaderBytecode as *const u8, b.BytecodeLength))
}

/// Writes a frame once: all of it, or (an error) none of it, so later frames stay readable.
fn put(j: &mut Journal, tag: u8, windows: u8, hash: u64, payload: &[u8]) -> std::io::Result<()> {
    if j.seen.contains(&(tag, hash)) {
        return Ok(());
    }
    let at = j.file.stream_position()?;
    if let Err(e) = j.file.write_all(&frame(tag, windows, hash, payload)) {
        let _ = j.file.set_len(at);
        let _ = j.file.seek(SeekFrom::Start(at));
        return Err(e);
    }
    j.seen.insert((tag, hash));
    Ok(())
}

/// A pipeline's frames, built and hashed with no lock held: its shaders', then its own.
struct Entry<'a> {
    shaders: Vec<Out<'a>>,
    tag: u8,
    windows: u8,
    hash: u64,
    payload: Vec<u8>,
}

impl Entry<'_> {
    fn frames(&self) -> Vec<Out<'_>> {
        let mut f = self.shaders.clone();
        f.push((self.tag, self.windows, self.hash, &self.payload));
        f
    }
}

/// A shader's hash into the pipeline's payload, and its frame (none for an empty slot).
fn add_shader<'a>(shaders: &mut Vec<Out<'a>>, out: &mut Vec<u8>, sh: Option<&'a [u8]>) {
    let h = sh.map_or(0, crate::gpu::shader::fnv64);
    if let Some(sh) = sh {
        shaders.push((b'S', 0, h, sh));
    }
    out.extend_from_slice(&h.to_le_bytes());
}

/// `pso::mem_windows` in a frame's byte: the count in the low 7 bits, `HEAP_WINDOWS` in the top
/// bit (the shaders take the windows from the descriptor heap: other HLSL than the switch form).
pub fn windows_byte(mem_windows: u32) -> u8 {
    (mem_windows & 0x7F) as u8 | if mem_windows & crate::gpu::recomp::HEAP_WINDOWS != 0 { 0x80 } else { 0 }
}

/// A frame's memory windows as `pso::mem_windows` had them (`windows_byte`).
pub fn mem_windows_of(byte: u8) -> u32 {
    (byte & 0x7F) as u32 | if byte & 0x80 != 0 { crate::gpu::recomp::HEAP_WINDOWS } else { 0 }
}

fn entry(shaders: Vec<Out>, tag: u8, payload: Vec<u8>) -> Entry {
    // the memory windows are part of what the pipeline is (its HLSL differs with them, and with
    // heap windows)
    let windows = windows_byte(crate::gpu::recomp::pso::mem_windows());
    entry_with_windows(shaders, tag, payload, windows)
}

fn entry_with_windows(shaders: Vec<Out>, tag: u8, mut payload: Vec<u8>, windows: u8) -> Entry {
    payload.push(windows);
    let hash = crate::gpu::shader::fnv64(&payload);
    payload.pop();
    Entry { shaders, tag, windows, hash, payload }
}

/// Writes the entry; its hash.
fn note_entry(e: Entry) -> u64 {
    // Live lookup still needs the descriptor identity when recording is disabled.
    if !enabled() { return e.hash; }
    write(&e.frames());
    note_creation(e.hash, e.windows);
    e.hash
}

fn note_creation(hash: u64, windows: u8) {
    static CREATIONS: std::sync::LazyLock<Mutex<HashMap<u64, u32>>> = std::sync::LazyLock::new(Default::default);
    let ordinal = {
        let mut counts = CREATIONS.lock().unwrap();
        let count = counts.entry(hash).or_default();
        *count = count.saturating_add(1);
        *count
    };
    if ordinal > 1 {
        let mut payload = [0u8; 12];
        payload[..8].copy_from_slice(&hash.to_le_bytes());
        payload[8..].copy_from_slice(&ordinal.to_le_bytes());
        write(&[(b'R', windows, crate::gpu::shader::fnv64(&payload), &payload)]);
    }
}

/// Note a graphics pipeline; its hash for `used`, 0 = not written.
pub unsafe fn graphics(d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC) -> u64 {
    note_entry(graphics_entry(d, windows_byte(crate::gpu::recomp::pso::mem_windows())))
}

/// Older captures can precede the stream's console-to-PC render-target conversion.
/// The native builders perform that same conversion for ordinary and two-pass PSOs.
/// Keep the exact journal identity; use the equivalent PC descriptor for live matching.
pub(super) unsafe fn graphics_live_key(d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC, recorded: u64) -> u64 {
    graphics_live_key_in(d, recorded, windows_byte(crate::gpu::recomp::pso::mem_windows()))
}

unsafe fn graphics_live_key_in(d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC, recorded: u64, windows: u8) -> u64 {
    let active = d.NumRenderTargets.min(8) as usize;
    if !d.RTVFormats[..active].iter().any(|f| super::device_x::pc_target_format(f.0 as u32) != f.0 as u32) {
        return recorded;
    }
    let mut copy = std::ptr::read(d);
    for f in &mut copy.RTVFormats[..active] {
        f.0 = super::device_x::pc_target_format(f.0 as u32) as i32;
    }
    graphics_entry(&copy, windows).hash
}

unsafe fn graphics_entry(d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC, windows: u8) -> Entry<'_> {
    graphics_entry_with_depth_bounds(d, windows, false)
}

pub(super) unsafe fn graphics_depth_bounds(d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC) -> u64 {
    note_entry(graphics_entry_with_depth_bounds(d, windows_byte(crate::gpu::recomp::pso::mem_windows()), true))
}

unsafe fn graphics_entry_with_depth_bounds(d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC, windows: u8, depth_bounds: bool) -> Entry<'_> {
    let (mut shaders, mut p) = (Vec::new(), Vec::new());
    for b in [&d.VS, &d.PS, &d.DS, &d.HS, &d.GS] {
        add_shader(&mut shaders, &mut p, bytes_of(b));
    }
    let desc = graphics_bytes(d);
    p.extend_from_slice(&(desc.len() as u32).to_le_bytes());
    p.extend_from_slice(&desc);
    let elems = if d.InputLayout.NumElements > 0 && !d.InputLayout.pInputElementDescs.is_null() {
        std::slice::from_raw_parts(d.InputLayout.pInputElementDescs, d.InputLayout.NumElements as usize)
    } else {
        &[]
    };
    p.extend_from_slice(&(elems.len() as u32).to_le_bytes());
    for e in elems {
        let name = std::ffi::CStr::from_ptr(e.SemanticName.0 as *const std::ffi::c_char).to_bytes();
        p.extend_from_slice(&(name.len() as u32).to_le_bytes());
        p.extend_from_slice(name);
        for w in [e.SemanticIndex, e.Format.0 as u32, e.InputSlot, e.AlignedByteOffset, e.InputSlotClass.0 as u32, e.InstanceDataStepRate] {
            p.extend_from_slice(&w.to_le_bytes());
        }
    }
    if depth_bounds { p.extend_from_slice(b"DB01"); }
    entry_with_windows(shaders, b'G', p, windows)
}

/// Note a compute pipeline; its hash for `used`, 0 = not written.
pub unsafe fn compute(d: &D3D12_COMPUTE_PIPELINE_STATE_DESC) -> u64 {
    let source = bytes_of(&d.CS);
    let hash = source.map_or(0, crate::gpu::shader::fnv64);
    let windows = windows_byte(crate::gpu::recomp::pso::mem_windows());
    let (key, payload) = compute_recipe(hash, d, windows);
    let shaders = source.into_iter().map(|s| (b'S', 0, hash, s)).collect();
    note_entry(Entry { shaders, tag: b'C', windows, hash: key, payload })
}

pub(crate) fn compute_recipe(source: u64, d: &D3D12_COMPUTE_PIPELINE_STATE_DESC, windows: u8) -> (u64, Vec<u8>) {
    let mut p = source.to_le_bytes().to_vec();
    let desc = compute_bytes(d);
    p.extend_from_slice(&(desc.len() as u32).to_le_bytes());
    p.extend_from_slice(&desc);
    let e = entry_with_windows(Vec::new(), b'C', p, windows);
    (e.hash, e.payload)
}

/// Mesh streams replace the console root signature in the translator. Persist fixed-function
/// values and stage hashes only. A nonempty pointer-bearing subobject needs its own encoder.
pub(super) fn mesh_stream(buf: &[u8], blobs: &[Vec<u8>]) -> u64 {
    mesh_entry(buf, blobs).map_or(0, note_entry)
}

/// X records preserve a refused stream for inspection without scheduling it for preparation.
pub(super) fn refused_mesh_stream(buf: &[u8], blobs: &[Vec<u8>]) -> bool {
    let Some(mut entry) = mesh_entry(buf, blobs) else { return false; };
    entry.tag = b'X';
    write(&entry.frames());
    true
}

fn mesh_entry<'a>(buf: &[u8], blobs: &'a [Vec<u8>]) -> Option<Entry<'a>> {
    mesh_entry_slots(buf, blobs, false)
}

/// Look up an explicitly prebuilt mesh recipe for a console stream using the AS tag.
/// Only a single unambiguous slot is eligible. Never write a new demand record on a miss.
#[cfg(test)]
pub(super) fn numbered_mesh_key(buf: &[u8], blobs: &[Vec<u8>]) -> Option<u64> {
    mesh_entry_slots(buf, blobs, true).map(|entry| entry.hash)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumberedMesh { Off, Prepared, All }

pub(crate) fn numbered_mesh_mode() -> NumberedMesh {
    static MODE: std::sync::OnceLock<NumberedMesh> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("XCL_PREPARED_NUMBERED_MESH").as_deref() {
        Ok("1") => NumberedMesh::Prepared,
        Ok("all") => NumberedMesh::All,
        _ => NumberedMesh::Off,
    })
}

pub(super) fn numbered_mesh_request(buf: &[u8], blobs: &[Vec<u8>]) -> Option<u64> {
    let e = mesh_entry_slots(buf, blobs, true)?;
    if enabled() { note_creation(e.hash, e.windows); }
    Some(e.hash)
}

// These bytes are ABI padding copied from the title, and may differ across launches.
// Use the Windows field offsets so every meaningful blend and depth/stencil field survives.
fn clear_numbered_mesh_padding(ty: u32, payload: &mut [u8]) {
    use std::mem::{offset_of, size_of};
    if ty == 8 {
        let targets = offset_of!(D3D12_BLEND_DESC, RenderTarget);
        let stride = size_of::<D3D12_RENDER_TARGET_BLEND_DESC>();
        let end = offset_of!(D3D12_RENDER_TARGET_BLEND_DESC, RenderTargetWriteMask) + 1;
        for target in payload[targets..].chunks_exact_mut(stride) { target[end..].fill(0); }
    } else if ty == 21 {
        let start = offset_of!(D3D12_DEPTH_STENCIL_DESC1, StencilWriteMask) + 1;
        let end = offset_of!(D3D12_DEPTH_STENCIL_DESC1, FrontFace);
        payload[start..end].fill(0);
    }
}

/// Recover the equivalent MS descriptor from an older diagnostic capture, only when
/// its sole AS reference is an actual mesh shader. The caller owns the copied payload.
pub(crate) fn captured_numbered_mesh(mut wire: Vec<u8>, windows: u8, hash: u64,
    mut is_mesh: impl FnMut(u64) -> bool) -> Option<(u64, Vec<u8>)> {
    wire.push(windows);
    if crate::gpu::shader::fnv64(&wire) != hash { return None; }
    wire.pop();
    let mut at = 0;
    let mut slot = None;
    while at < wire.len() {
        let start = at;
        let ty = u32_at(&wire, &mut at)?;
        let (size, _) = super::null::subobject_payload(ty)?;
        match ty {
            25 => return None,
            0 | 7 | 12 | 19 | 22 => {},
            1..=6 | 24 => {
                let source = u64::from_le_bytes(wire.get(at..at+8)?.try_into().ok()?);
                if source != 0 && !matches!(ty, 2 | 24) { return None; }
                if ty == 24 {
                    if slot.is_some() || source == 0 || !is_mesh(source) { return None; }
                    slot = Some(start);
                }
                at += 8;
            }
            _ => { clear_numbered_mesh_padding(ty, wire.get_mut(at..at+size)?); at += size; }
        }
    }
    let at = slot?;
    wire[at..at+4].copy_from_slice(&25u32.to_le_bytes());
    let e = entry_with_windows(Vec::new(), b'M', wire, windows);
    Some((e.hash, e.payload))
}

fn mesh_entry_slots<'a>(buf: &[u8], blobs: &'a [Vec<u8>], numbered: bool) -> Option<Entry<'a>> {
    let mut stages = blobs.iter();
    let (mut refs, mut wire) = (Vec::new(), Vec::new());
    let mut off = 0;
    let mut replaced = false;
    while off < buf.len() {
        let ty = u32::from_le_bytes(buf.get(off..off + 4)?.try_into().ok()?);
        let (size, align) = super::null::subobject_payload(ty)?;
        let start = off + if align == 8 { 8 } else { 4 };
        let payload = buf.get(start..start + size)?;
        let wire_type = if numbered && ty == 24 {
            if replaced { return None; }
            replaced = true;
            25
        } else {
            if numbered && ty == 25 { return None; }
            ty
        };
        wire.extend_from_slice(&wire_type.to_le_bytes());
        match ty {
            0 | 19 => {} // guest root signature and driver-cache hint are replaced
            1..=6 | 24 | 25 => {
                let len = usize::from_le_bytes(payload.get(8..16)?.try_into().ok()?);
                if numbered && len != 0 && !matches!(ty, 2 | 24) { return None; }
                let shader = if len == 0 { None } else {
                    let b = stages.next()?;
                    if b.len() != len { return None; }
                    Some(b.as_slice())
                };
                add_shader(&mut refs, &mut wire, shader);
            }
            7 | 12 | 22 => if payload.iter().any(|&b| b != 0) { return None; },
            _ => {
                let start = wire.len();
                wire.extend_from_slice(payload);
                if numbered && numbered_mesh_mode() == NumberedMesh::All {
                    clear_numbered_mesh_padding(ty, &mut wire[start..]);
                }
            }
        }
        off = (start + size + 7) & !7;
    }
    if off != buf.len() || stages.next().is_some() || (numbered && !replaced) { return None; }
    Some(entry(refs, b'M', wire))
}

/// Recreate the PC-aligned subobjects with every pointer targeting owned storage.
fn replay_mesh(wire: &[u8], shader: impl Fn(u64) -> Option<Arc<Vec<u8>>>) -> Option<Replay> {
    let (mut words, mut shaders) = (Vec::<u64>::new(), Vec::new());
    let mut at = 0;
    while at < wire.len() {
        let ty = u32_at(wire, &mut at)?;
        let (size, align) = super::null::subobject_payload(ty)?;
        let offset = if align == 8 { 8 } else { 4 };
        let base = words.len();
        words.resize(base + (offset + size).div_ceil(8), 0);
        let out = unsafe { std::slice::from_raw_parts_mut(words[base..].as_mut_ptr().cast::<u8>(), offset + size) };
        out[..4].copy_from_slice(&ty.to_le_bytes());
        match ty {
            0 | 7 | 12 | 19 | 22 => {}
            1..=6 | 24 | 25 => {
                let hash = u64::from_le_bytes(wire.get(at..at + 8)?.try_into().ok()?);
                at += 8;
                if hash != 0 {
                    let bytes = shader(hash)?;
                    out[offset..offset + 8].copy_from_slice(&(bytes.as_ptr() as usize).to_le_bytes());
                    out[offset + 8..].copy_from_slice(&bytes.len().to_le_bytes());
                    shaders.push(bytes);
                }
            }
            _ => {
                out[offset..].copy_from_slice(wire.get(at..at + size)?);
                at += size;
            }
        }
    }
    Some(Replay::Mesh(words, shaders))
}

/// Pipeline `hash` was built (the title used it): `xcl pipelines` makes these first.
pub fn used(hash: u64) {
    if hash == 0 {
        return;
    }
    write(&[(b'U', 0, hash, &[])]);
}

/// First bind or submit that needs this recipe, including requests whose build never finishes.
pub fn demanded(hash: u64) {
    if hash == 0 { return; }
    write(&[(b'N', 0, hash, &[])]);
}

/// A journal's pipeline, read back with everything it points to owned.
use std::sync::Arc;

pub enum Replay {
    Graphics(Box<D3D12_GRAPHICS_PIPELINE_STATE_DESC>, Vec<Arc<Vec<u8>>>, Vec<D3D12_INPUT_ELEMENT_DESC>, Vec<std::ffi::CString>, bool),
    Compute(Box<D3D12_COMPUTE_PIPELINE_STATE_DESC>, Vec<Arc<Vec<u8>>>),
    Mesh(Vec<u64>, Vec<Arc<Vec<u8>>>),
}

unsafe impl Send for Replay {}

fn read_desc<T>(p: &[u8], at: &mut usize) -> Option<Box<T>> {
    let n = u32::from_le_bytes(p.get(*at..*at + 4)?.try_into().ok()?) as usize;
    *at += 4;
    let bytes = p.get(*at..*at + n)?;
    *at += n;
    if n != std::mem::size_of::<T>() {
        return None;
    }
    unsafe {
        let mut b: Box<std::mem::MaybeUninit<T>> = Box::new(std::mem::MaybeUninit::zeroed());
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), b.as_mut_ptr() as *mut u8, n);
        Some(Box::from_raw(Box::into_raw(b) as *mut T))
    }
}

fn u32_at(p: &[u8], at: &mut usize) -> Option<u32> {
    let v = u32::from_le_bytes(p.get(*at..*at + 4)?.try_into().ok()?);
    *at += 4;
    Some(v)
}

/// The pipeline of frame `f` (`G` / `C`), its shaders from `shader` by hash. None: malformed,
/// or a shader not in the journal.
pub fn replay(f: &Frame, shader: impl Fn(u64) -> Option<Vec<u8>>) -> Option<Replay> {
    replay_verified(f, |key| {
        let bytes = shader(key)?;
        (crate::gpu::shader::fnv64(&bytes) == key).then(|| Arc::new(bytes))
    })
}

/// The source table has already verified each immutable allocation against its key.
pub(crate) fn replay_verified(f: &Frame, shader: impl Fn(u64) -> Option<Arc<Vec<u8>>>) -> Option<Replay> {
    let p = f.payload;
    let digest = crate::gpu::shader::fnv64(p);
    if (digest ^ u64::from(f.windows)).wrapping_mul(0x100_0000_01b3) != f.hash { return None; }
    let mut at = 0usize;
    let sh = |at: &mut usize| -> Option<Option<Arc<Vec<u8>>>> {
        let h = u64::from_le_bytes(p.get(*at..*at + 8)?.try_into().ok()?);
        *at += 8;
        if h == 0 {
            return Some(None);
        }
        Some(Some(shader(h)?))
    };
    let code = |b: &Vec<u8>| D3D12_SHADER_BYTECODE { pShaderBytecode: b.as_ptr() as *const _, BytecodeLength: b.len() };
    match f.tag {
        b'M' | b'X' => replay_mesh(p, shader),
        b'C' => {
            let cs = sh(&mut at)??;
            let mut d: Box<D3D12_COMPUTE_PIPELINE_STATE_DESC> = read_desc(p, &mut at)?;
            // (no pointer from the file reaches the driver)
            d.pRootSignature = std::mem::ManuallyDrop::new(None);
            d.CachedPSO = D3D12_CACHED_PIPELINE_STATE::default();
            if at != p.len() { return None; }
            d.CS = code(&cs);
            Some(Replay::Compute(d, vec![cs]))
        }
        b'G' => {
            let mut blobs = Vec::new();
            let mut slots = [None; 5];
            for s in slots.iter_mut() {
                if let Some(b) = sh(&mut at)? {
                    blobs.push(b);
                    *s = Some(blobs.len() - 1);
                }
            }
            let mut d: Box<D3D12_GRAPHICS_PIPELINE_STATE_DESC> = read_desc(p, &mut at)?;
            // (no pointer from the file reaches the driver)
            d.pRootSignature = std::mem::ManuallyDrop::new(None);
            d.StreamOutput = unsafe { std::mem::zeroed() };
            d.CachedPSO = D3D12_CACHED_PIPELINE_STATE::default();
            for b in [&mut d.VS, &mut d.PS, &mut d.DS, &mut d.HS, &mut d.GS] {
                *b = D3D12_SHADER_BYTECODE::default();
            }
            for (s, b) in slots.iter().zip([&mut d.VS, &mut d.PS, &mut d.DS, &mut d.HS, &mut d.GS]) {
                if let Some(i) = s {
                    *b = code(&blobs[*i]);
                }
            }
            let n = u32_at(p, &mut at)? as usize;
            if d.NumRenderTargets > 8 || n != d.InputLayout.NumElements as usize || n > 32 || n > (p.len() - at) / 28 {
                return None; // each element: a name length and six words at least
            }
            let mut names = Vec::with_capacity(n);
            let mut raw = Vec::with_capacity(n);
            for _ in 0..n {
                let len = u32_at(p, &mut at)? as usize;
                names.push(std::ffi::CString::new(p.get(at..at + len)?).ok()?);
                at += len;
                let mut w = [0u32; 6];
                for x in w.iter_mut() {
                    *x = u32_at(p, &mut at)?;
                }
                raw.push(w);
            }
            let depth_bounds = match &p[at..] {
                [] => false,
                b"DB01" => true,
                _ => return None,
            };
            let elems: Vec<D3D12_INPUT_ELEMENT_DESC> = names
                .iter()
                .zip(&raw)
                .map(|(name, w)| D3D12_INPUT_ELEMENT_DESC {
                    SemanticName: windows::core::PCSTR(name.as_ptr() as *const u8),
                    SemanticIndex: w[0],
                    Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(w[1] as i32),
                    InputSlot: w[2],
                    AlignedByteOffset: w[3],
                    InputSlotClass: D3D12_INPUT_CLASSIFICATION(w[4] as i32),
                    InstanceDataStepRate: w[5],
                })
                .collect();
            d.InputLayout = D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: if elems.is_empty() { std::ptr::null() } else { elems.as_ptr() }, NumElements: elems.len() as u32 };
            Some(Replay::Graphics(d, blobs, elems, names, depth_bounds))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_bounds_has_a_distinct_recipe_and_survives_replay() {
        let d = D3D12_GRAPHICS_PIPELINE_STATE_DESC::default();
        for enabled in [false, true] {
            let entry = unsafe { graphics_entry_with_depth_bounds(&d, 4, enabled) };
            let plain = unsafe { graphics_entry(&d, 4) };
            assert_eq!(entry.hash == plain.hash, !enabled);
            let f = Frame { tag: entry.tag, windows: entry.windows, hash: entry.hash, payload: &entry.payload };
            let Some(Replay::Graphics(_, _, _, _, decoded)) = replay(&f, |_| None) else { panic!("invalid graphics recipe") };
            assert_eq!(decoded, enabled);
        }
    }

    #[test]
    fn captured_console_target_matches_its_pc_pipeline_without_changing_the_record() {
        use windows::Win32::Graphics::Dxgi::Common::*;
        let mut d = D3D12_GRAPHICS_PIPELINE_STATE_DESC { NumRenderTargets: 1, ..Default::default() };
        d.RTVFormats[0] = DXGI_FORMAT_R9G9B9E5_SHAREDEXP;
        unsafe {
            let original = graphics_entry(&d, 0x84).hash;
            let mapped = graphics_live_key_in(&d, original, 0x84);
            assert_eq!(d.RTVFormats[0], DXGI_FORMAT_R9G9B9E5_SHAREDEXP);
            assert_ne!(original, mapped);
            d.RTVFormats[0] = DXGI_FORMAT_R11G11B10_FLOAT;
            assert_eq!(mapped, graphics_entry(&d, 0x84).hash);
            assert_eq!(mapped, graphics_live_key_in(&d, mapped, 0x84));
            assert_ne!(mapped, graphics_entry(&d, 0x04).hash);
            d.RTVFormats[0] = DXGI_FORMAT_R32_FLOAT;
            assert_ne!(mapped, graphics_entry(&d, 0x84).hash);
        }
    }

    #[test]
    fn captured_pipeline_descriptors_round_trip() {
        let Some(path) = std::env::var_os("XCL_SAMPLE_PIPELINE_JOURNAL") else { return };
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.starts_with(MAGIC));
        let mut shaders = HashMap::new();
        let (mut graphics, mut compute, mut mesh) = (0, 0, 0);
        let mut mapped_targets = Vec::new();
        let mut graphics_keys = HashSet::new();
        let end = for_each_frame(&bytes, |f| {
            if f.tag == b'S' {
                assert_eq!(crate::gpu::shader::fnv64(f.payload), f.hash);
                shaders.insert(f.hash, f.payload);
                return;
            }
            if !matches!(f.tag, b'G' | b'C' | b'M') { return; }
            let recipe = replay(&f, |h| shaders.get(&h).map(|v| v.to_vec())).expect("captured recipe must replay");
            let payload = match &recipe {
                Replay::Graphics(d, _, _, _, depth_bounds) => {
                    graphics += 1;
                    graphics_keys.insert(f.hash);
                    let live = unsafe { graphics_live_key_in(d, f.hash, f.windows) };
                    if live != f.hash { mapped_targets.push(live); }
                    unsafe { graphics_entry_with_depth_bounds(d, f.windows, *depth_bounds) }.payload
                }
                Replay::Compute(d, source) => {
                    compute += 1;
                    compute_recipe(crate::gpu::shader::fnv64(&source[0]), d, f.windows).1
                }
                Replay::Mesh(words, sources) => {
                    mesh += 1;
                    let buf = unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), std::mem::size_of_val(words.as_slice())) };
                    mesh_entry(buf, &sources.iter().map(|b| (**b).clone()).collect::<Vec<_>>()).unwrap().payload
                }
            };
            assert_eq!(payload, f.payload, "recipe {:016x}", f.hash);
        });
        assert_eq!(end, bytes.len(), "expected a whole capture");
        assert!(graphics + compute + mesh > 0);
        println!("Round-tripped {graphics} graphics, {compute} compute, {mesh} mesh and {} shader sources", shaders.len());
        println!("{} mapped render-target recipes also captured in PC form", mapped_targets.iter().filter(|k| graphics_keys.contains(*k)).count());
    }

    #[test]
    fn the_heap_windows_flag_is_kept() {
        let heap = crate::gpu::recomp::HEAP_WINDOWS;
        for w in [1, 4, 4 | heap, 16 | heap] {
            assert_eq!(mem_windows_of(windows_byte(w)), w);
        }
        assert_ne!(windows_byte(4), windows_byte(4 | heap));
    }

    #[test]
    fn numbered_mesh_padding_does_not_split_live_identities() {
        let recipe = |padding, write_mask, stencil_mask| {
            let mut wire = Vec::from(24u32.to_le_bytes());
            wire.extend_from_slice(&7u64.to_le_bytes());
            wire.extend_from_slice(&8u32.to_le_bytes());
            let mut blend = [0u8; 328];
            for rt in 0..8 {
                let mask = 8 + rt*40 + 36;
                blend[mask] = write_mask;
                blend[mask+1..mask+4].fill(padding);
            }
            wire.extend_from_slice(&blend);
            wire.extend_from_slice(&21u32.to_le_bytes());
            let mut depth = [0u8; 56];
            depth[17] = stencil_mask;
            depth[18..20].fill(padding);
            wire.extend_from_slice(&depth);
            let e = entry_with_windows(Vec::new(), b'X', wire, 132);
            captured_numbered_mesh(e.payload, e.windows, e.hash, |s| s == 7).unwrap()
        };
        let a = recipe(0, 15, 255);
        assert_eq!(a, recipe(93, 15, 255));
        assert_ne!(a.0, recipe(93, 7, 255).0);
        assert_ne!(a.0, recipe(93, 15, 127).0);
    }

    #[test]
    fn a_refused_mesh_recipe_retains_the_as_and_pixel_slots() {
        let stages = [vec![1u8, 2], vec![3u8, 4, 5]];
        let mut stream = Vec::new();
        for (ty, bytes) in [(24u64, &stages[0]), (2, &stages[1])] {
            for word in [ty, bytes.as_ptr() as u64, bytes.len() as u64] {
                stream.extend_from_slice(&word.to_le_bytes());
            }
        }
        let e = mesh_entry(&stream, &stages).unwrap();
        let numbered = numbered_mesh_key(&stream, &stages).unwrap();
        let source = crate::gpu::shader::fnv64(&stages[0]);
        let projected = captured_numbered_mesh(e.payload.clone(), e.windows, e.hash, |h| h == source).unwrap();
        assert_eq!(projected.0, numbered);
        assert!(captured_numbered_mesh(e.payload.clone(), e.windows, e.hash, |_| false).is_none());
        assert!(captured_numbered_mesh(e.payload.clone(), e.windows, e.hash ^ 1, |_| true).is_none());
        let mut extra = e.payload.clone();
        extra.extend_from_slice(&5u32.to_le_bytes());
        extra.extend_from_slice(&source.to_le_bytes());
        let with_geometry = entry_with_windows(Vec::new(), b'X', extra, e.windows);
        assert!(captured_numbered_mesh(with_geometry.payload, e.windows, with_geometry.hash, |_| true).is_none());
        let mut pc = stream.clone();
        pc[..4].copy_from_slice(&25u32.to_le_bytes());
        assert_eq!(numbered, mesh_entry(&pc, &stages).unwrap().hash);
        assert_eq!(projected.1, mesh_entry(&pc, &stages).unwrap().payload);
        assert_ne!(numbered, e.hash);
        assert!(numbered_mesh_key(&pc, &stages).is_none());
        for other in [24u64, 25] {
            let mut ambiguous = stream.clone();
            for word in [other, 0, 0] { ambiguous.extend_from_slice(&word.to_le_bytes()); }
            assert!(numbered_mesh_key(&ambiguous, &stages).is_none());
        }
        let frame = Frame { tag: b'X', windows: e.windows, hash: e.hash, payload: &e.payload };
        let Some(Replay::Mesh(words, owned)) = replay(&frame, |h| stages.iter().find(|s| crate::gpu::shader::fnv64(s) == h).cloned()) else {
            panic!("refused mesh descriptor missing");
        };
        assert_eq!(words.as_slice(), &[24, owned[0].as_ptr() as u64, 2, 2, owned[1].as_ptr() as u64, 3]);
        assert!(owned.iter().map(Arc::as_ref).eq(stages.iter()));
    }

    #[test]
    fn mesh_stream_replay_owns_its_stage_and_excludes_process_pointers() {
        let stage = vec![1u8, 2, 3];
        let stage_hash = crate::gpu::shader::fnv64(&stage);
        let mut buf = Vec::new();
        for word in [0u64, 0xedededededededed, 25, stage.as_ptr() as u64, 3] {
            buf.extend_from_slice(&word.to_le_bytes());
        }
        let blobs = [stage.clone()];
        let e = mesh_entry(&buf, &blobs).unwrap();
        assert_eq!(e.payload, [0u32.to_le_bytes().as_slice(), &25u32.to_le_bytes(), &stage_hash.to_le_bytes()].concat());
        let frame = Frame { tag: e.tag, windows: e.windows, hash: e.hash, payload: &e.payload };
        let Some(Replay::Mesh(words, owned)) = replay(&frame, |h| (h == stage_hash).then(|| stage.clone())) else { panic!("mesh not restored") };
        assert_eq!(words.as_slice(), &[0, 0, 25, owned[0].as_ptr() as u64, 3]);
        assert!(mesh_entry(&buf[..buf.len()-1], &blobs).is_none());
        assert!(mesh_entry(&buf, &[]).is_none());
    }

    #[test]
    fn graphics_round_trip() {
        let vs = vec![1u8, 2, 3, 4];
        let ps = vec![9u8; 40];
        let name = std::ffi::CString::new("TEXCOORD").unwrap();
        let elems = [D3D12_INPUT_ELEMENT_DESC { SemanticName: windows::core::PCSTR(name.as_ptr() as *const u8), SemanticIndex: 3, InputSlot: 1, AlignedByteOffset: 12, ..Default::default() }];
        let mut d = D3D12_GRAPHICS_PIPELINE_STATE_DESC::default();
        d.VS = D3D12_SHADER_BYTECODE { pShaderBytecode: vs.as_ptr() as *const _, BytecodeLength: vs.len() };
        d.PS = D3D12_SHADER_BYTECODE { pShaderBytecode: ps.as_ptr() as *const _, BytecodeLength: ps.len() };
        d.InputLayout = D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: elems.as_ptr(), NumElements: 1 };
        d.NumRenderTargets = 2;
        d.SampleMask = 0xffff_ffff;
        let mut j = Journal { file: tempfile("graphics"), seen: HashSet::new() };
        let h1 = unsafe { graphics_in(&mut j, &d).unwrap() };
        let h2 = unsafe { graphics_in(&mut j, &d).unwrap() };
        assert_eq!(h1, h2, "the same desc made again: the same hash");
        // garbage in the padding after a render target's write mask (a desc built on the stack)
        let mut d3: D3D12_GRAPHICS_PIPELINE_STATE_DESC = unsafe { std::ptr::read(&d) };
        let pad = std::mem::offset_of!(D3D12_GRAPHICS_PIPELINE_STATE_DESC, BlendState.RenderTarget)
            + std::mem::offset_of!(D3D12_RENDER_TARGET_BLEND_DESC, RenderTargetWriteMask)
            + 1;
        unsafe { *(&mut d3 as *mut D3D12_GRAPHICS_PIPELINE_STATE_DESC as *mut u8).add(pad) = 0xAA };
        assert_eq!(unsafe { graphics_in(&mut j, &d3).unwrap() }, h1, "padding is not part of the pipeline");
        std::mem::forget(d3);
        let mut bytes = MAGIC.to_vec();
        use std::io::{Read, Seek};
        j.file.seek(std::io::SeekFrom::Start(0)).unwrap();
        j.file.read_to_end(&mut bytes).unwrap();
        let mut frames = Vec::new();
        for_each_frame(&bytes, |f| frames.push((f.tag, f.hash, f.payload.to_vec())));
        assert_eq!(frames.iter().filter(|f| f.0 == b'S').count(), 2);
        assert_eq!(frames.iter().filter(|f| f.0 == b'G').count(), 1, "written once");
        let g = frames.iter().find(|f| f.0 == b'G').unwrap();
        let shader = |h| frames.iter().find(|f| f.0 == b'S' && f.1 == h).map(|f| f.2.clone());
        let windows = windows_byte(crate::gpu::recomp::pso::mem_windows());
        let frame = Frame { tag: b'G', windows, hash: g.1, payload: &g.2 };
        assert!(replay(&Frame { hash: g.1 ^ 1, ..frame }, shader).is_none());
        assert!(replay(&frame, |_| Some(vec![42])).is_none());
        let Some(Replay::Graphics(r, blobs, _elems, _names, false)) = replay(&frame, shader) else { panic!("no replay") };
        assert!(blobs.iter().map(Arc::as_ref).eq([&vs, &ps]));
        unsafe {
            assert_eq!(bytes_of(&r.VS).unwrap(), &vs[..]);
            assert_eq!(bytes_of(&r.PS).unwrap(), &ps[..]);
            assert!(bytes_of(&r.GS).is_none());
            assert_eq!((r.NumRenderTargets, r.SampleMask, r.InputLayout.NumElements), (2, 0xffff_ffff, 1));
            let e = &*r.InputLayout.pInputElementDescs;
            assert_eq!((e.SemanticName.to_string().unwrap().as_str(), e.SemanticIndex, e.InputSlot, e.AlignedByteOffset), ("TEXCOORD", 3, 1, 12));
        }
        // a torn tail is not a frame
        let whole = for_each_frame(&bytes, |_| {});
        assert_eq!(for_each_frame(&bytes[..bytes.len() - 3], |_| {}) < whole, true);
    }

    #[test]
    fn graphics_replay_preserves_geometry_shader_pair() {
        let vs = [1u8, 2, 3];
        let first_gs = [4u8, 5];
        let second_gs = [6u8, 7];
        let bytecode = |code: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len(),
        };
        let mut d = D3D12_GRAPHICS_PIPELINE_STATE_DESC::default();
        d.VS = bytecode(&vs);
        d.GS = bytecode(&first_gs);
        let first_hash = unsafe { graphics_entry(&d, 1) }.hash;
        d.GS = bytecode(&second_gs);
        let second = unsafe { graphics_entry(&d, 1) };
        assert_ne!(first_hash, second.hash);
        let frame = Frame { tag: b'G', windows: 1, hash: second.hash, payload: &second.payload };
        let load = |key| second.shaders.iter().find(|s| s.2 == key).map(|s| s.3.to_vec());
        assert!(replay(&frame, |key| if key == crate::gpu::shader::fnv64(&second_gs) { None } else { load(key) }).is_none());
        let Some(Replay::Graphics(desc, _blobs, _elems, _names, false)) = replay(&frame, load) else { panic!("missing geometry replay") };
        unsafe {
            assert_eq!(bytes_of(&desc.VS), Some(vs.as_slice()));
            assert_eq!(bytes_of(&desc.GS), Some(second_gs.as_slice()));
            assert!(bytes_of(&desc.PS).is_none());
        }
    }

    /// The entry written straight to `j` (no global state); its hash.
    unsafe fn graphics_in(j: &mut Journal, d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC) -> std::io::Result<u64> {
        let e = graphics_entry(d, windows_byte(crate::gpu::recomp::pso::mem_windows()));
        e.frames().iter().try_for_each(|&(t, w, h, p)| put(j, t, w, h, p))?;
        Ok(e.hash)
    }

    #[test]
    fn what_is_noted_while_the_journal_opens_is_written_after_in_order() {
        let (vs, ps) = (vec![1u8; 8], vec![2u8; 8]);
        let mut g = State::Closed;
        assert!(note(&mut g, &[(b'S', 0, 11, &vs[..]), (b'G', 0, 21, &[7])]), "the first note starts the opening");
        assert!(!note(&mut g, &[(b'S', 0, 11, &vs[..]), (b'S', 0, 12, &ps[..]), (b'G', 0, 22, &[8])]), "only once");
        assert!(!note(&mut g, &[(b'U', 0, 21, &[])]));
        let State::Opening(q, _, bytes) = &g else { panic!("not opening") };
        assert_eq!((q.len(), *bytes), (5, 18), "a shader noted twice is kept once");
        opened(&mut g, Some(Journal { file: tempfile("opening"), seen: HashSet::new() }));
        let State::Open(j) = &mut g else { panic!("not open") };
        let mut bytes = MAGIC.to_vec();
        use std::io::{Read, Seek};
        j.file.seek(std::io::SeekFrom::Start(0)).unwrap();
        j.file.read_to_end(&mut bytes).unwrap();
        let mut order = Vec::new();
        for_each_frame(&bytes, |f| order.push((f.tag, f.hash)));
        assert_eq!(order, [(b'S', 11), (b'G', 21), (b'S', 12), (b'G', 22), (b'U', 21)]);
    }

    fn tempfile(case: &str) -> std::fs::File {
        let p = std::env::temp_dir().join(format!("xcl_pso_journal_{}_{case}.bin", std::process::id()));
        let f = std::fs::OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        f
    }
}
