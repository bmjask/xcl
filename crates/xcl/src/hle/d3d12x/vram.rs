//! GPU-owned textures in title memory, aliased as on the console.
//!
//! A console title places render targets, depth buffers and UAV textures at addresses of its
//! own memory and reuses that memory: transient targets of a frame share it, and it creates
//! several resource objects over the same address expecting one memory (seen on XDefiant: 16
//! placements at an occupied address in its first seconds, also of other formats and sizes).
//! A committed PC resource per placement breaks that – what the GPU writes through one never
//! shows in the other. So these textures are placed in *arenas*: D3D12 default heaps (video
//! memory) standing for ranges of the title's address space (128 MiB+), at the offset of their
//! address in it. Resources at one address then share memory as on the console (the same
//! description: the same PC layout, so the same texels). The CPU never sees these textures
//! (on the console it could, but GPU-owned targets are not CPU-written).

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::Mutex;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use crate::util::HRESULT;

/// bytes of title address space an arena stands for at least (and of video memory it takes)
const ARENA: usize = 128 << 20;
/// arenas start and end at multiples of this
const ARENA_ALIGN: usize = 32 << 20;

struct Arena {
    base: usize,
    size: usize,
    heap: ID3D12Heap,
}

// D3D12 heaps are free-threaded
unsafe impl Send for Arena {}

static ARENAS: Mutex<Vec<Arena>> = Mutex::new(Vec::new());
static PLACED_TEXTURES: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

/// A texture backed by one of the aliasable D3D12 heaps.
pub(super) fn is_placed_texture(resource: usize) -> bool {
    PLACED_TEXTURES.lock().unwrap().as_ref().is_some_and(|textures| textures.contains(&resource))
}

pub(super) fn forget(resource: usize) {
    let placed = PLACED_TEXTURES.lock().unwrap().as_mut().is_some_and(|textures| textures.remove(&resource));
    if placed {
        forget_pages(resource);
    }
}

/// Whether a console texture with these D3D12_RESOURCE_FLAGS is the GPU's (render target,
/// depth buffer, UAV).
pub fn gpu_owned(flags: u32) -> bool {
    flags & 0x7 != 0
}

/// Where a placement at `[va, va + bytes)` goes, given the arenas' ranges (base, size).
#[derive(Debug, PartialEq)]
enum Plan {
    /// into arena `index`, at `offset`
    Reuse { index: usize, offset: usize },
    /// into a new arena `[base, base + size)`, which takes the place of the arenas `replaces`
    /// (indices, ascending) in the list
    New { base: usize, size: usize, replaces: Vec<usize> },
}

/// The arenas never overlap, so an address has one arena and resources at one address share
/// memory. A new arena takes the 32 MiB blocks the range needs and, while it overlaps none,
/// grows to 128 MiB – no further than the next arena. A range that straddles arenas gets one
/// arena over the union of the range and every arena it overlaps; those arenas leave the list
/// (their resources keep their heaps, but later placements at their addresses use the new one).
fn plan(arenas: &[(usize, usize)], va: usize, bytes: usize) -> Plan {
    if let Some(index) = arenas.iter().position(|&(b, n)| b <= va && va + bytes <= b + n) {
        return Plan::Reuse { index, offset: va - arenas[index].0 };
    }
    let (mut lo, mut hi) = (va & !(ARENA_ALIGN - 1), (va + bytes).div_ceil(ARENA_ALIGN) * ARENA_ALIGN);
    let overlaps = |lo: usize, hi: usize| -> Vec<usize> { (0..arenas.len()).filter(|&i| arenas[i].0 < hi && lo < arenas[i].0 + arenas[i].1).collect() };
    let mut replaces = overlaps(lo, hi);
    if replaces.is_empty() {
        let next = arenas.iter().map(|&(b, _)| b).filter(|&b| b >= hi).min().unwrap_or(usize::MAX);
        hi = hi.max(lo.saturating_add(ARENA)).min(next);
        return Plan::New { base: lo, size: hi - lo, replaces };
    }
    // the union grows as it takes in arenas, which may overlap further ones
    loop {
        for &i in &replaces {
            lo = lo.min(arenas[i].0);
            hi = hi.max(arenas[i].0 + arenas[i].1);
        }
        let more = overlaps(lo, hi);
        if more.len() == replaces.len() {
            return Plan::New { base: lo, size: hi - lo, replaces };
        }
        replaces = more;
    }
}

/// The arena holding `[va, va + bytes)` (None: no video memory).
unsafe fn arena_for(dev: &ID3D12Device, va: usize, bytes: usize) -> Option<(ID3D12Heap, u64)> {
    let mut g = ARENAS.lock().unwrap();
    let ranges: Vec<(usize, usize)> = g.iter().map(|a| (a.base, a.size)).collect();
    let (base, size, replaces) = match plan(&ranges, va, bytes) {
        Plan::Reuse { index, offset } => return Some((g[index].heap.clone(), offset as u64)),
        Plan::New { base, size, replaces } => (base, size, replaces),
    };
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: size as u64,
        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
        // MSAA targets need 4 MiB placement
        Alignment: D3D12_DEFAULT_MSAA_RESOURCE_PLACEMENT_ALIGNMENT as u64,
        Flags: D3D12_HEAP_FLAG_NONE,
    };
    let mut heap: Option<ID3D12Heap> = None;
    if let Err(e) = dev.CreateHeap(&desc, &mut heap) {
        crate::xlog_warn!("D3D12.X: video-memory arena for {base:#x}: CreateHeap: {e}");
        return None;
    }
    let heap = heap?;
    if replaces.is_empty() {
        crate::xlog_info!("D3D12.X: video-memory arena for title addresses {base:#x}..{:#x}", base + size);
    } else {
        // the placed resources hold their heaps: the old arenas live on as long as they do
        crate::xlog_info!(
            "D3D12.X: video-memory arena for title addresses {base:#x}..{:#x} replaces {} arena(s) a placement at {va:#x}..{:#x} straddled (resources already in them no longer alias later ones)",
            base + size,
            replaces.len(),
            va + bytes
        );
        for &i in replaces.iter().rev() {
            g.remove(i);
        }
    }
    g.push(Arena { base, size, heap: heap.clone() });
    Some((heap, (va - base) as u64))
}

const PAGE: usize = 0x1_0000;
const POOL_PAGES: u32 = 1024;
const IDLE: std::time::Duration = std::time::Duration::from_secs(2);
const COOL: std::time::Duration = std::time::Duration::from_secs(2);
type Tile = (u32, u32);

struct Page {
    tile: Tile,
    users: u32,
    idle_since: Option<std::time::Instant>,
}

struct Pages {
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    value: u64,
    pools: Vec<ID3D12Heap>,
    free: Vec<Tile>,
    cooling: VecDeque<(Tile, std::time::Instant)>,
    pages: HashMap<usize, Page>,
    targets: HashMap<usize, (Vec<usize>, Vec<Tile>)>,
    waited: HashMap<usize, u64>,
}

unsafe impl Send for Pages {}

static PAGES: Mutex<Option<Option<Pages>>> = Mutex::new(None);

impl Pages {
    unsafe fn new(dev: &ID3D12Device) -> Option<Self> {
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        dev.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &mut options as *mut _ as *mut c_void, std::mem::size_of_val(&options) as u32).ok()?;
        if options.TiledResourcesTier.0 < D3D12_TILED_RESOURCES_TIER_2.0 {
            crate::xlog_info!("D3D12.X: tiled resources tier {}: GPU-owned textures remain in arenas", options.TiledResourcesTier.0);
            return None;
        }
        Some(Self {
            queue: dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).ok()?,
            fence: dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?,
            value: 0,
            pools: Vec::new(), free: Vec::new(), cooling: VecDeque::new(), pages: HashMap::new(), targets: HashMap::new(), waited: HashMap::new(),
        })
    }

    fn sweep(&mut self, now: std::time::Instant) {
        let old: Vec<_> = self.pages.iter().filter(|(_, p)| p.idle_since.is_some_and(|t| now.duration_since(t) >= IDLE)).map(|(&a, _)| a).collect();
        for address in old {
            if let Some(page) = self.pages.remove(&address) { self.cooling.push_back((page.tile, now)); }
        }
    }

    unsafe fn take_tile(&mut self, dev: &ID3D12Device) -> Option<Tile> {
        if let Some(tile) = self.free.pop() { return Some(tile); }
        if self.cooling.front().is_some_and(|(_, time)| time.elapsed() >= COOL) { return self.cooling.pop_front().map(|(tile, _)| tile); }
        let desc = D3D12_HEAP_DESC { SizeInBytes: POOL_PAGES as u64 * PAGE as u64, Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, Alignment: 0, Flags: D3D12_HEAP_FLAG_NONE };
        let mut heap = None;
        if let Err(error) = dev.CreateHeap(&desc, &mut heap) { crate::xlog_warn!("D3D12.X: no video memory for GPU-owned target pages ({error})"); return None; }
        self.pools.push(heap?);
        let index = self.pools.len() as u32 - 1;
        self.free.extend((1..POOL_PAGES).rev().map(|tile| (index, tile)));
        Some((index, 0))
    }

    unsafe fn map(&mut self, resource: &ID3D12Resource, tiles: &[Tile]) {
        let mut heaps: Vec<_> = tiles.iter().map(|tile| tile.0).collect();
        heaps.sort_unstable(); heaps.dedup();
        for heap in heaps {
            let ranges = heap_ranges(heap, tiles);
            let flags: Vec<_> = ranges.iter().map(|range| if range.0 { D3D12_TILE_RANGE_FLAG_NONE } else { D3D12_TILE_RANGE_FLAG_SKIP }).collect();
            let offsets: Vec<_> = ranges.iter().map(|range| range.1).collect();
            let counts: Vec<_> = ranges.iter().map(|range| range.2).collect();
            let coord = D3D12_TILED_RESOURCE_COORDINATE::default();
            let region = D3D12_TILE_REGION_SIZE { NumTiles: tiles.len() as u32, UseBox: false.into(), ..Default::default() };
            self.queue.UpdateTileMappings(resource, 1, Some(&coord), Some(&region), &self.pools[heap as usize], ranges.len() as u32, Some(flags.as_ptr()), Some(offsets.as_ptr()), Some(counts.as_ptr()), D3D12_TILE_MAPPING_FLAG_NONE);
        }
        self.value += 1;
        let _ = self.queue.Signal(&self.fence, self.value);
    }
}

fn heap_ranges(heap: u32, tiles: &[Tile]) -> Vec<(bool, u32, u32)> {
    let mut ranges: Vec<(bool, u32, u32)> = Vec::new();
    for &(tile_heap, tile) in tiles {
        let mine = tile_heap == heap;
        match ranges.last_mut() {
            Some(range) if mine && range.0 && range.1 + range.2 == tile => range.2 += 1,
            Some(range) if !mine && !range.0 => range.2 += 1,
            _ => ranges.push((mine, if mine { tile } else { 0 }, 1)),
        }
    }
    ranges
}

#[allow(clippy::too_many_arguments)]
unsafe fn place_on_pages(dev: &ID3D12Device, this: *mut c_void, va: usize, desc: &D3D12_RESOURCE_DESC, state: u32, clear: *const c_void, console_bytes: Option<u64>, riid: *const windows_sys::core::GUID, ppv: *mut *mut c_void) -> Option<HRESULT> {
    if va % PAGE != 0 || desc.Layout != D3D12_TEXTURE_LAYOUT_UNKNOWN { return None; }
    let mut lock = PAGES.lock().unwrap();
    let pages = lock.get_or_insert_with(|| Pages::new(dev)).as_mut()?;
    let tiled = D3D12_RESOURCE_DESC { Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE, ..*desc };
    let (hr, _) = super::device_x::retry_resource_with_common(|initial| super::real::dev_create_reserved_resource(this, &tiled as *const _ as usize, initial as usize, clear as usize, riid, ppv), state);
    if hr < 0 || (*ppv).is_null() {
        static FAILURES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
            crate::xlog_info!("D3D12.X: reserved target at {va:#x} ({}x{}, format {}, flags {:#x}, state {state:#x}) refused ({hr:#x})", desc.Width, desc.Height, desc.Format.0, desc.Flags.0);
        }
        return None;
    }
    let raw = *ppv;
    let resource = ID3D12Resource::from_raw_borrowed(&raw)?;
    let mut total = 0u32;
    dev.GetResourceTiling(resource, Some(&mut total), None, None, None, 0, std::ptr::null_mut());
    let own = console_bytes.map_or(total as usize, |bytes| (bytes as usize).div_ceil(PAGE)).min(total as usize);
    let now = std::time::Instant::now(); pages.sweep(now);
    let mut tiles = Vec::with_capacity(total as usize); let mut used = Vec::with_capacity(own); let mut extra = Vec::new();
    for index in 0..total as usize {
        let tile = if index < own {
            let address = va.checked_add(index.checked_mul(PAGE)?)?;
            match pages.pages.get_mut(&address) {
                Some(page) => { page.users += 1; page.idle_since = None; Some(page.tile) }
                None => pages.take_tile(dev).inspect(|&tile| { pages.pages.insert(address, Page { tile, users: 1, idle_since: None }); }),
            }.inspect(|_| used.push(address))
        } else { pages.take_tile(dev).inspect(|&tile| extra.push(tile)) };
        let Some(tile) = tile else { break }; tiles.push(tile);
    }
    pages.targets.insert(raw as usize, (used, extra));
    if tiles.len() < total as usize {
        drop(lock); forget_pages(raw as usize); drop(windows::core::IUnknown::from_raw(raw)); *ppv = std::ptr::null_mut(); return None;
    }
    pages.map(resource, &tiles);
    PLACED_TEXTURES.lock().unwrap().get_or_insert_with(HashSet::new).insert(raw as usize);
    Some(hr)
}

fn forget_pages(resource: usize) {
    let mut lock = PAGES.lock().unwrap();
    let Some(Some(pages)) = lock.as_mut() else { return };
    let Some((used, extra)) = pages.targets.remove(&resource) else { return };
    let now = std::time::Instant::now();
    for address in used { if let Some(page) = pages.pages.get_mut(&address) { page.users = page.users.saturating_sub(1); if page.users == 0 { page.idle_since = Some(now); } } }
    pages.cooling.extend(extra.into_iter().map(|tile| (tile, now)));
}

pub(super) unsafe fn before_execute(queue: &ID3D12CommandQueue) {
    let mut lock = PAGES.lock().unwrap();
    let Some(Some(pages)) = lock.as_mut() else { return };
    let key = queue.as_raw() as usize;
    if pages.waited.get(&key).copied().unwrap_or(0) < pages.value { let _ = queue.Wait(&pages.fence, pages.value); pages.waited.insert(key, pages.value); }
}

/// Heaps of every kind of resource in one (resource heap tier 2) – else no arenas.
fn tier2(dev: &ID3D12Device) -> bool {
    let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    unsafe { dev.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &mut o as *mut _ as *mut c_void, std::mem::size_of_val(&o) as u32).is_ok() && o.ResourceHeapTier.0 >= 2 }
}

/// A GPU-owned texture of the title at `va`, preferentially as a tiled resource backed by
/// its console-address pages; falls back to an arena where tiled resources are unavailable.
#[allow(clippy::too_many_arguments)]
pub unsafe fn place(this: *mut c_void, va: usize, desc: &D3D12_RESOURCE_DESC, state: u32, clear: *const c_void, console_bytes: Option<u64>, riid: *const windows_sys::core::GUID, ppv: *mut *mut c_void) -> Option<HRESULT> {
    let dev = ID3D12Device::from_raw_borrowed(&this)?;
    if !tier2(dev) {
        return None;
    }
    if std::env::var_os("XCL_VRAM_ARENAS").is_none() {
        if let Some(hr) = place_on_pages(dev, this, va, desc, state, clear, console_bytes, riid, ppv) {
            return Some(hr);
        }
    }
    let info = dev.GetResourceAllocationInfo(0, &[*desc]);
    if info.SizeInBytes == u64::MAX || info.SizeInBytes == 0 || va as u64 % info.Alignment.max(1) != 0 {
        return None;
    }
    let (heap, offset) = arena_for(dev, va, info.SizeInBytes as usize)?;
    let (hr, _effective_state) = super::device_x::retry_resource_with_common(
        |initial| super::real::dev_create_placed_resource(this, heap.as_raw() as usize, offset as usize, desc as *const _ as usize, initial as usize, clear as usize, riid, ppv),
        state,
    );
    if hr < 0 || (*ppv).is_null() {
        return None;
    }
    PLACED_TEXTURES.lock().unwrap().get_or_insert_with(HashSet::new).insert(*ppv as usize);
    Some(hr)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1 << 20;

    #[test]
    fn tile_ranges_per_heap() {
        let tiles = [(0, 5), (0, 6), (1, 0), (1, 1), (0, 9), (0, 10), (0, 11)];
        assert_eq!(heap_ranges(0, &tiles), [(true, 5, 2), (false, 0, 2), (true, 9, 3)]);
        assert_eq!(heap_ranges(1, &tiles), [(false, 0, 2), (true, 0, 2), (false, 0, 3)]);
        assert_eq!(heap_ranges(0, &[(0, 3), (0, 1), (0, 2)]), [(true, 3, 1), (true, 1, 2)]);
    }

    /// Applies placements to a list of arena ranges as `arena_for` does; returns the arena
    /// (base) each placement landed in.
    fn place_all(seq: &[(usize, usize)]) -> (Vec<(usize, usize)>, Vec<usize>) {
        let mut arenas: Vec<(usize, usize)> = Vec::new();
        let mut landed = Vec::new();
        for &(va, bytes) in seq {
            match plan(&arenas, va, bytes) {
                Plan::Reuse { index, offset } => {
                    assert_eq!(arenas[index].0 + offset, va);
                    landed.push(arenas[index].0);
                }
                Plan::New { base, size, replaces } => {
                    assert!(base <= va && va + bytes <= base + size, "{va:#x} in {base:#x}+{size:#x}");
                    assert_eq!((base % ARENA_ALIGN, size % ARENA_ALIGN), (0, 0));
                    for &i in replaces.iter().rev() {
                        arenas.remove(i);
                    }
                    arenas.push((base, size));
                    landed.push(base);
                }
            }
            for (i, a) in arenas.iter().enumerate() {
                for b in &arenas[i + 1..] {
                    assert!(a.0 + a.1 <= b.0 || b.0 + b.1 <= a.0, "arenas {a:x?} and {b:x?} overlap");
                }
            }
        }
        (arenas, landed)
    }

    #[test]
    fn a_straddling_placement_gets_one_arena_over_both() {
        // #104: 16 MiB at 0x1_0400_0000, then 48 MiB at 0x1_0200_0000 (ends inside the first arena)
        let (arenas, landed) = place_all(&[(0x1_0400_0000, 16 * MIB), (0x1_0200_0000, 48 * MIB), (0x1_0400_0000, 16 * MIB)]);
        assert_eq!(arenas, [(0x1_0200_0000, 0x1_0C00_0000 - 0x1_0200_0000)]);
        // the third placement, at the first one's address, shares the new arena with the second
        assert_eq!(landed[1], landed[2]);
    }

    #[test]
    fn a_new_arena_stops_at_the_next_one() {
        // 0x1_0400_0000 + 128 MiB first; then 16 MiB below it fits in the 32 MiB gap
        let (arenas, landed) = place_all(&[(0x1_0400_0000, 16 * MIB), (0x1_0200_0000, 16 * MIB)]);
        assert_eq!(arenas, [(0x1_0400_0000, ARENA), (0x1_0200_0000, 32 * MIB)]);
        assert_eq!(landed, [0x1_0400_0000, 0x1_0200_0000]);
    }

    #[test]
    fn a_placement_over_several_arenas_merges_them_all() {
        // two arenas with a gap, then one placement over the gap into both
        let a = 0x2_0000_0000;
        let (arenas, _) = place_all(&[(a, MIB), (a + 160 * MIB, MIB), (a + 100 * MIB, 100 * MIB), (a + 50 * MIB, MIB)]);
        assert_eq!(arenas, [(a, 160 * MIB + ARENA)]);
    }
}
