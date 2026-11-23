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

use std::collections::HashSet;
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
    if let Some(textures) = PLACED_TEXTURES.lock().unwrap().as_mut() {
        textures.remove(&resource);
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

/// Heaps of every kind of resource in one (resource heap tier 2) – else no arenas.
fn tier2(dev: &ID3D12Device) -> bool {
    let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    unsafe { dev.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &mut o as *mut _ as *mut c_void, std::mem::size_of_val(&o) as u32).is_ok() && o.ResourceHeapTier.0 >= 2 }
}

/// A GPU-owned texture of the title at `va` (`desc`: the PC description): placed in the arena
/// for that address (None: it cannot be – the caller makes a resource of its own).
pub unsafe fn place(this: *mut c_void, va: usize, desc: &D3D12_RESOURCE_DESC, state: u32, clear: *const c_void, riid: *const windows_sys::core::GUID, ppv: *mut *mut c_void) -> Option<HRESULT> {
    let dev = ID3D12Device::from_raw_borrowed(&this)?;
    if !tier2(dev) {
        return None;
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
