//! GPU-owned textures in title memory, aliased as on the console.
//!
//! A console title places render targets, depth buffers and UAV textures at addresses of its
//! own memory and reuses that memory: transient targets of a frame share it, and it creates
//! several resource objects over the same address expecting one memory (seen on XDefiant: 16
//! placements at an occupied address in its first seconds, also of other formats and sizes).
//! A committed PC resource per placement breaks that – what the GPU writes through one never
//! shows in the other. So these textures are placed in *arenas*: D3D12 default heaps (video
//! memory) sized for ranges of the title's address space, at the offset of their
//! address in it. Resources at one address then share memory as on the console (the same
//! description: the same PC layout, so the same texels). The CPU never sees these textures
//! (on the console it could, but GPU-owned targets are not CPU-written).

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::Mutex;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use crate::util::{E_INVALIDARG, HRESULT};

/// A heap of video memory for placements at title addresses `[base, base + size)`.
struct Arena<H> {
    base: usize,
    size: usize,
    align: usize,
    heap: H,
    users: u32,
    idle_since: Option<std::time::Instant>,
    id: u64,
}

/// Arena bookkeeping is separate from D3D12 to keep placement and reclamation testable.
struct Arenas<H> {
    list: Vec<Arena<H>>,
    next_id: u64,
}

impl<H: Clone> Arenas<H> {
    const fn new() -> Self { Self { list: Vec::new(), next_id: 0 } }

    fn sweep(&mut self, now: std::time::Instant) {
        self.list.retain(|arena| arena.users > 0 || arena.idle_since.is_none_or(|idle| now.duration_since(idle) < IDLE));
    }

    fn take(&mut self, va: usize, bytes: usize, align: usize, now: std::time::Instant, make: impl FnOnce(usize, usize) -> Option<H>) -> Option<(H, usize, u64)> {
        if align == 0 { return None; }
        self.sweep(now);
        let end = va.checked_add(bytes);
        let fits = |arena: &Arena<H>| arena.align >= align && arena.base <= va && end.is_some_and(|end| arena.base.checked_add(arena.size).is_some_and(|arena_end| end <= arena_end)) && (va - arena.base) % align == 0;
        let index = match self.list.iter().rposition(fits) {
            Some(index) => index,
            None => {
                let size = bytes.checked_add(align - 1)? / align;
                let size = size.checked_mul(align)?;
                let heap = make(size, align)?;
                self.next_id += 1;
                self.list.push(Arena { base: va, size, align, heap, users: 0, idle_since: None, id: self.next_id });
                self.list.len() - 1
            }
        };
        let arena = &mut self.list[index];
        arena.users += 1;
        arena.idle_since = None;
        Some((arena.heap.clone(), va - arena.base, arena.id))
    }

    fn release(&mut self, id: u64, now: std::time::Instant) {
        if let Some(arena) = self.list.iter_mut().find(|arena| arena.id == id) {
            arena.users = arena.users.saturating_sub(1);
            if arena.users == 0 { arena.idle_since = Some(now); }
        }
    }

    #[cfg(test)]
    fn bytes(&self) -> usize { self.list.iter().map(|arena| arena.size).sum() }
}

// D3D12 heaps are free-threaded.
unsafe impl Send for Arenas<ID3D12Heap> {}

static ARENAS: Mutex<Arenas<ID3D12Heap>> = Mutex::new(Arenas::new());
static ARENA_OF: Mutex<Option<HashMap<usize, u64>>> = Mutex::new(None);
static PLACED_TEXTURES: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

/// A texture backed by one of the aliasable D3D12 heaps.
pub(super) fn is_placed_texture(resource: usize) -> bool {
    PLACED_TEXTURES.lock().unwrap().as_ref().is_some_and(|textures| textures.contains(&resource))
}

pub(super) fn forget(resource: usize) {
    let placed = PLACED_TEXTURES.lock().unwrap().as_mut().is_some_and(|textures| textures.remove(&resource));
    if placed {
        forget_pages(resource);
        if let Some(id) = ARENA_OF.lock().unwrap().as_mut().and_then(|arenas| arenas.remove(&resource)) {
            ARENAS.lock().unwrap().release(id, std::time::Instant::now());
        }
    }
}

/// Whether a console texture with these D3D12_RESOURCE_FLAGS is the GPU's (render target,
/// depth buffer, UAV).
pub fn gpu_owned(flags: u32) -> bool {
    flags & 0x7 != 0
}

unsafe fn arena_heap(dev: &ID3D12Device, va: usize, size: usize, align: usize) -> Option<ID3D12Heap> {
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: size as u64,
        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
        Alignment: align as u64,
        Flags: D3D12_HEAP_FLAG_NONE,
    };
    let mut heap: Option<ID3D12Heap> = None;
    if let Err(e) = dev.CreateHeap(&desc, &mut heap) {
        crate::xlog_warn!("D3D12.X: video-memory arena for {va:#x}: CreateHeap: {e}");
        return None;
    }
    crate::xlog_info!("D3D12.X: placement-sized video-memory arena for {va:#x}..{:#x}", va + size);
    heap
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

/// The optimized clear value is optional. Some drivers refuse a console-derived value even
/// though the resource itself is valid, so retry without that optimization.
pub(super) fn retry_without_clear(mut create: impl FnMut(u32, *const c_void) -> HRESULT, state: u32, clear: *const c_void) -> (HRESULT, u32, bool) {
    let (first, effective_state) = super::device_x::retry_resource_with_common(|initial| create(initial, clear), state);
    if first != E_INVALIDARG || clear.is_null() {
        return (first, effective_state, false);
    }
    let (fallback, effective_state) = super::device_x::retry_resource_with_common(|initial| create(initial, std::ptr::null()), state);
    (fallback, effective_state, fallback >= 0)
}

#[allow(clippy::too_many_arguments)]
unsafe fn place_on_pages(dev: &ID3D12Device, this: *mut c_void, va: usize, desc: &D3D12_RESOURCE_DESC, state: u32, clear: *const c_void, console_bytes: Option<u64>, riid: *const windows_sys::core::GUID, ppv: *mut *mut c_void) -> Option<HRESULT> {
    if va % PAGE != 0 || desc.Layout != D3D12_TEXTURE_LAYOUT_UNKNOWN { return None; }
    let mut lock = PAGES.lock().unwrap();
    let pages = lock.get_or_insert_with(|| Pages::new(dev)).as_mut()?;
    let tiled = D3D12_RESOURCE_DESC { Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE, ..*desc };
    let (hr, _, ignored_clear) = retry_without_clear(
        |initial, optimized_clear| {
            *ppv = std::ptr::null_mut();
            super::real::dev_create_reserved_resource(this, &tiled as *const _ as usize, initial as usize, optimized_clear as usize, riid, ppv)
        },
        state,
        clear,
    );
    if ignored_clear {
        static IGNORED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if IGNORED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
            crate::xlog_info!("D3D12.X: reserved target at {va:#x} accepted without its optional clear-value optimization");
        }
    }
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
    let alignment = (info.Alignment as usize).max(PAGE);
    let now = std::time::Instant::now();
    let (heap, offset, arena) = ARENAS.lock().unwrap().take(va, info.SizeInBytes as usize, alignment, now, |size, align| arena_heap(dev, va, size, align))?;
    let (hr, _, ignored_clear) = retry_without_clear(
        |initial, optimized_clear| super::real::dev_create_placed_resource(this, heap.as_raw() as usize, offset, desc as *const _ as usize, initial as usize, optimized_clear as usize, riid, ppv),
        state,
        clear,
    );
    if ignored_clear {
        crate::xlog_info!("D3D12.X: placement at {va:#x} accepted without its optional clear-value optimization");
    }
    if hr < 0 || (*ppv).is_null() {
        ARENAS.lock().unwrap().release(arena, now);
        return None;
    }
    PLACED_TEXTURES.lock().unwrap().get_or_insert_with(HashSet::new).insert(*ppv as usize);
    ARENA_OF.lock().unwrap().get_or_insert_with(HashMap::new).insert(*ppv as usize, arena);
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

    #[test]
    fn reserved_target_retries_without_an_invalid_clear_value() {
        let clear = 1usize as *const c_void;
        let mut calls = Vec::new();
        let (hr, effective_state, ignored_clear) = retry_without_clear(
            |state, value| {
                calls.push((state, value.is_null()));
                if value.is_null() && state == 0 { 0 } else { E_INVALIDARG }
            },
            0xC0,
            clear,
        );
        assert_eq!(hr, 0);
        assert!(ignored_clear);
        assert_eq!((effective_state, calls), (0, vec![(0xC0, false), (0, false), (0xC0, true), (0, true)]));
    }

    #[test]
    fn reserved_target_does_not_drop_a_clear_on_other_errors() {
        let clear = 1usize as *const c_void;
        let mut calls = Vec::new();
        let (hr, effective_state, ignored_clear) = retry_without_clear(
            |state, value| { calls.push((state, value.is_null())); -1 },
            0xC0,
            clear,
        );
        assert_eq!(hr, -1);
        assert!(!ignored_clear);
        assert_eq!((effective_state, calls), (0xC0, vec![(0xC0, false)]));
    }

    #[test]
    fn null_clear_value_does_not_retry_the_same_resource_creation() {
        let mut calls = Vec::new();
        let (hr, effective_state, ignored_clear) = retry_without_clear(
            |state, value| { calls.push((state, value.is_null())); E_INVALIDARG },
            0xC0,
            std::ptr::null(),
        );
        assert_eq!((hr, effective_state, ignored_clear, calls), (E_INVALIDARG, 0, false, vec![(0xC0, true), (0, true)]));
    }

    fn take(arena: &mut Arenas<u32>, va: usize, bytes: usize, align: usize, now: std::time::Instant) -> u64 {
        let heap = arena.list.len() as u32;
        arena.take(va, bytes, align, now, |_, _| Some(heap)).unwrap().2
    }

    #[test]
    fn placement_sized_arenas_share_only_compatible_ranges() {
        let now = std::time::Instant::now();
        let base = 0x1_0000_0000;
        let mut arenas = Arenas::new();
        assert!(arenas.take(base, MIB, 0, now, |_, _| Some(0)).is_none());
        let first = take(&mut arenas, base, 70 * MIB, PAGE, now);
        assert_eq!(arenas.bytes(), 70 * MIB);
        assert_eq!(take(&mut arenas, base + 4 * MIB, 8 * MIB, PAGE, now), first);
        let straddling = take(&mut arenas, base + 64 * MIB, 70 * MIB, PAGE, now);
        let msaa = take(&mut arenas, base + 8 * MIB, 8 * MIB, 4 * MIB, now);
        assert!(straddling != first && msaa != first && msaa != straddling);
        assert_eq!(arenas.bytes(), (70 + 70 + 8) * MIB);
        assert_eq!(take(&mut arenas, base + 64 * MIB, 4 * MIB, PAGE, now), straddling);
    }

    #[test]
    fn unused_arena_is_reclaimed_after_idle_period() {
        let now = std::time::Instant::now();
        let mut arenas = Arenas::new();
        let id = take(&mut arenas, 0x1_0000_0000, 70 * MIB, PAGE, now);
        arenas.release(id, now);
        assert_eq!(take(&mut arenas, 0x1_0000_0000, 70 * MIB, PAGE, now + std::time::Duration::from_secs(1)), id);
        arenas.release(id, now + std::time::Duration::from_secs(1));
        take(&mut arenas, 0x1_2000_0000, MIB, PAGE, now + std::time::Duration::from_secs(1) + IDLE + std::time::Duration::from_millis(1));
        assert_eq!(arenas.bytes(), MIB);
    }

    #[test]
    fn streaming_placements_remain_bounded() {
        const SIZE: usize = 64 * MIB;
        let now = std::time::Instant::now();
        let mut arenas = Arenas::new();
        let mut live = std::collections::VecDeque::new();
        for round in 0..128usize {
            let at = now + std::time::Duration::from_millis(500 * round as u64);
            let start = (round * SIZE / 3) % (512 * MIB);
            let ids: Vec<_> = (0..8).map(|slot| take(&mut arenas, 0x2_0000_0000 + start + slot * SIZE, SIZE, PAGE, at)).collect();
            live.push_back(ids);
            if live.len() > 3 {
                for id in live.pop_front().unwrap() { arenas.release(id, at); }
            }
            assert!(arenas.bytes() <= 7 * 8 * SIZE, "{} bytes", arenas.bytes());
        }
    }
}
