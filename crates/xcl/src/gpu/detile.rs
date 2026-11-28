//! GPU console-texture detiling. The first stage is a format-independent byte gather into a
//! linear buffer; a later copy uses ordinary D3D12 texture footprints, including BC formats.

use super::tiling::{Chain, GpuLayout, MipLevel};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::{
    *,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

/// Exact 16-byte-packed constant-buffer layout used by [`HLSL`]. Addresses are offsets within
/// the bound raw source/destination buffers, so batches can suballocate both resources.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Constants {
    pub source_base: u32,
    pub destination_base: u32,
    pub output_bytes: u32,
    pub elem_log2: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub pitch: u32,
    pub origin_x: u32,
    pub origin_y: u32,
    pub slice: u32,
    pub block_w: u32,
    pub block_h: u32,
    pub block_d: u32,
    pub block_bytes: u32,
    pub base_xor: u32,
    pub table_offsets_01: [u32; 4],
    pub table_offsets_2: [u32; 2],
    pub _padding: [u32; 2],
    pub table_lengths_01: [u32; 4],
    pub table_lengths_2: [u32; 2],
}

impl Constants {
    pub fn new(layout: &GpuLayout, level: &MipLevel, source_base: u32, destination_base: u32, slice: u32) -> Option<Self> {
        let elem_bytes = 1usize << layout.elem_log2;
        let output_bytes = level.width.checked_mul(level.height)?.checked_mul(level.depth)?.checked_mul(elem_bytes)?;
        Some(Self {
            source_base: source_base.checked_add(level.offset.try_into().ok()?)?,
            destination_base,
            output_bytes: output_bytes.try_into().ok()?,
            elem_log2: layout.elem_log2,
            width: level.width.try_into().ok()?,
            height: level.height.try_into().ok()?,
            depth: level.depth.try_into().ok()?,
            pitch: level.pitch.try_into().ok()?,
            origin_x: level.origin.0.try_into().ok()?,
            origin_y: level.origin.1.try_into().ok()?,
            slice,
            block_w: layout.block_w,
            block_h: layout.block_h,
            block_d: layout.block_d,
            block_bytes: layout.block_bytes,
            base_xor: layout.base_xor,
            table_offsets_01: layout.table_offsets[..4].try_into().unwrap(),
            table_offsets_2: layout.table_offsets[4..].try_into().unwrap(),
            _padding: [0; 2],
            table_lengths_01: layout.table_lengths[..4].try_into().unwrap(),
            table_lengths_2: layout.table_lengths[4..].try_into().unwrap(),
        })
    }
}

/// Plan one dispatch per destination row so every UAV dword is contiguous even when a D3D12
/// placed footprint pads rows to 256 bytes. `slice_pitch` is row pitch times footprint rows.
pub fn rows(
    layout: &GpuLayout,
    level: &MipLevel,
    source_base: u32,
    destination_base: u32,
    row_pitch: u32,
    slice_pitch: u32,
    slice_bytes: u32,
    volume: bool,
) -> Option<Vec<Constants>> {
    let mut out = Vec::with_capacity(level.height.checked_mul(level.depth)?);
    for z in 0..level.depth {
        for y in 0..level.height {
            let mut row = *level;
            row.height = 1;
            row.depth = 1;
            row.origin.1 = row.origin.1.checked_add(y)?;
            let layer = if volume { z / layout.block_d as usize } else { 0 };
            let source = source_base.checked_add((layer as u32).checked_mul(slice_bytes)?)?;
            let destination = destination_base
                .checked_add((z as u32).checked_mul(slice_pitch)?)?
                .checked_add((y as u32).checked_mul(row_pitch)?)?;
            out.push(Constants::new(layout, &row, source, destination, z.try_into().ok()?)?);
        }
    }
    Some(out)
}

/// Reusable GPU byte-detile pipeline. Resource transitions and lifetime retention belong to the
/// caller so this can be recorded into the same DIRECT/COMPUTE batch as texture uploads.
pub struct Pipeline {
    root: ID3D12RootSignature,
    state: ID3D12PipelineState,
}

unsafe impl Send for Pipeline {}
unsafe impl Sync for Pipeline {}

impl Pipeline {
    pub unsafe fn new(device: &ID3D12Device) -> Result<Self, String> {
        let dxil = super::recomp::dxc::compile(HLSL, "main", "cs_6_0", &[])?;
        let root = device.CreateRootSignature(0, &dxil).map_err(|e| e.to_string())?;
        let shader = D3D12_SHADER_BYTECODE { pShaderBytecode: dxil.as_ptr() as _, BytecodeLength: dxil.len() };
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: shader,
            ..Default::default()
        };
        let state = device.CreateComputePipelineState(&desc).map_err(|e| e.to_string())?;
        Ok(Self { root, state })
    }

    pub unsafe fn record(
        &self,
        list: &ID3D12GraphicsCommandList,
        tiled: &ID3D12Resource,
        tables: &ID3D12Resource,
        linear: &ID3D12Resource,
        constants: &Constants,
    ) {
        list.SetPipelineState(&self.state);
        list.SetComputeRootSignature(&self.root);
        list.SetComputeRootShaderResourceView(0, tiled.GetGPUVirtualAddress());
        list.SetComputeRootShaderResourceView(1, tables.GetGPUVirtualAddress());
        list.SetComputeRootUnorderedAccessView(2, linear.GetGPUVirtualAddress());
        list.SetComputeRoot32BitConstants(3, (std::mem::size_of::<Constants>() / 4) as u32, constants as *const _ as _, 0);
        list.Dispatch(constants.output_bytes.div_ceil(4).div_ceil(64), 1, 1);
    }
}

/// Objects referenced by an asynchronous GPU-detile submission. The caller must retain this
/// until the queue fence which follows the submission completes.
pub struct InFlight {
    _alloc: ID3D12CommandAllocator,
    _list: ID3D12GraphicsCommandList,
    _pipeline: Pipeline,
    _tiled: ID3D12Resource,
    _tables: ID3D12Resource,
    _linear: ID3D12Resource,
    pub bytes: u64,
}

unsafe fn buffer(
    device: &ID3D12Device,
    bytes: u64,
    heap: D3D12_HEAP_TYPE,
    state: D3D12_RESOURCE_STATES,
    flags: D3D12_RESOURCE_FLAGS,
) -> Result<ID3D12Resource, String> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes.max(256).checked_next_multiple_of(4).ok_or("GPU detile buffer size overflow")?,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    };
    let mut resource = None;
    device
        .CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() },
            D3D12_HEAP_FLAG_NONE,
            &desc,
            state,
            None,
            &mut resource,
        )
        .map_err(|e| e.to_string())?;
    resource.ok_or_else(|| "GPU detile buffer creation returned no resource".into())
}

unsafe fn upload(device: &ID3D12Device, bytes: &[u8]) -> Result<ID3D12Resource, String> {
    let resource = buffer(device, bytes.len() as u64, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ, D3D12_RESOURCE_FLAG_NONE)?;
    let mut mapped = std::ptr::null_mut();
    resource.Map(0, None, Some(&mut mapped)).map_err(|e| e.to_string())?;
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapped as *mut u8, bytes.len());
    resource.Unmap(0, None);
    Ok(resource)
}

fn transition(resource: &ID3D12Resource, subresource: u32, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: subresource,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// Detile one console texture into padded D3D12 footprints, then copy those footprints into
/// `destination`. This records and submits only; queue/fence ordering and retirement belong to
/// the caller. `raw` is bounded by the title-upload batch before this function is called.
pub unsafe fn submit(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    destination: &ID3D12Resource,
    chain: &Chain,
    slices: usize,
    raw: &[u8],
    states: &[D3D12_RESOURCE_STATES],
) -> Result<InFlight, String> {
    let desc = destination.GetDesc();
    let subresources = if chain.levels.first().is_some_and(|l| l.depth > 1) { chain.levels.len() } else { chain.levels.len().checked_mul(slices).ok_or("GPU detile subresource overflow")? };
    if states.len() != 1 && states.len() != subresources {
        return Err("GPU detile state count does not match texture".into());
    }
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); subresources];
    let mut row_counts = vec![0u32; subresources];
    let mut total = 0u64;
    device.GetCopyableFootprints(&desc, 0, subresources as u32, 0, Some(footprints.as_mut_ptr()), Some(row_counts.as_mut_ptr()), None, Some(&mut total));
    if total == u64::MAX || total > u32::MAX as u64 || raw.len() > u32::MAX as usize {
        return Err("GPU detile staging exceeds 32-bit shader addressing".into());
    }
    let layout = chain.layout.gpu_layout();
    let table_bytes = std::slice::from_raw_parts(layout.tables.as_ptr() as *const u8, layout.tables.len() * 4);
    let tiled = upload(device, raw)?;
    let tables = upload(device, table_bytes)?;
    let linear = buffer(device, total, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS)?;
    let pipeline = Pipeline::new(device)?;
    let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(|e| e.to_string())?;
    let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).map_err(|e| e.to_string())?;
    let volume = subresources == chain.levels.len() && chain.levels.first().is_some_and(|l| l.depth > 1);
    for sub in 0..subresources {
        let mip = sub % chain.levels.len();
        let array = sub / chain.levels.len();
        let level = &chain.levels[mip];
        let fp = footprints[sub];
        let source = if volume { 0 } else { array.checked_mul(chain.slice_bytes).ok_or("GPU detile source offset overflow")? };
        let coordinate = if volume { 0 } else { array };
        let plans = rows(
            &layout,
            level,
            source.try_into().map_err(|_| "GPU detile source offset overflow")?,
            fp.Offset.try_into().map_err(|_| "GPU detile footprint offset overflow")?,
            fp.Footprint.RowPitch,
            fp.Footprint.RowPitch.checked_mul(row_counts[sub]).ok_or("GPU detile slice pitch overflow")?,
            chain.slice_bytes.try_into().map_err(|_| "GPU detile slice size overflow")?,
            volume,
        ).ok_or("GPU detile row plan overflow")?;
        for mut constants in plans {
            constants.slice = constants.slice.checked_add(coordinate.try_into().map_err(|_| "GPU detile slice overflow")?).ok_or("GPU detile slice overflow")?;
            pipeline.record(&list, &tiled, &tables, &linear, &constants);
        }
    }
    let uav = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 { UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::transmute_copy(&linear) }) },
    };
    list.ResourceBarrier(&[uav, transition(&linear, D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
    for sub in 0..subresources {
        let state = if states.len() == 1 { states[0] } else { states[sub] };
        if state != D3D12_RESOURCE_STATE_COPY_DEST {
            list.ResourceBarrier(&[transition(destination, sub as u32, state, D3D12_RESOURCE_STATE_COPY_DEST)]);
        }
        let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(destination), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: sub as u32 } };
        let src = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&linear), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprints[sub] } };
        list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        if state != D3D12_RESOURCE_STATE_COPY_DEST {
            list.ResourceBarrier(&[transition(destination, sub as u32, D3D12_RESOURCE_STATE_COPY_DEST, state)]);
        }
    }
    list.Close().map_err(|e| e.to_string())?;
    let command = list.cast().map_err(|e| e.to_string())?;
    crate::hle::d3d12x::real::native_queue_execute(queue, &command);
    Ok(InFlight { _alloc: alloc, _list: list, _pipeline: pipeline, _tiled: tiled, _tables: tables, _linear: linear, bytes: raw.len() as u64 + total + table_bytes.len() as u64 })
}

/// One thread writes one aligned output dword. Each byte is gathered independently so 1/2-byte
/// elements and swizzles whose neighbouring elements are non-contiguous need no UAV atomics.
pub const HLSL: &str = r#"
#define RS "SRV(t0), SRV(t1), UAV(u0), RootConstants(num32BitConstants=30, b0)"
ByteAddressBuffer tiled : register(t0);
StructuredBuffer<uint> tables : register(t1);
RWByteAddressBuffer linear : register(u0);

cbuffer Detile : register(b0) {
    uint src_base, dst_base, output_bytes, elem_log2;
    uint width, height, depth, pitch;
    uint origin_x, origin_y, slice, block_w;
    uint block_h, block_d, block_bytes, base_xor;
    uint4 table_offsets_01;
    uint2 table_offsets_2;
    uint4 table_lengths_01;
    uint2 table_lengths_2;
};

uint lookup(uint kind, uint index) {
    uint off = kind < 4 ? table_offsets_01[kind] : table_offsets_2[kind - 4];
    uint len = kind < 4 ? table_lengths_01[kind] : table_lengths_2[kind - 4];
    return tables[off + index % len];
}

uint source_element(uint x, uint y, uint z) {
    x += origin_x;
    y += origin_y;
    z += slice;
    uint pitch_blocks = (pitch + block_w - 1) / block_w;
    uint block = (y / block_h) * pitch_blocks + x / block_w;
    uint v = lookup(0, x % block_w) ^ lookup(1, y % block_h) ^ lookup(2, z % block_d)
           ^ lookup(3, x / block_w) ^ lookup(4, y / block_h) ^ lookup(5, z / block_d);
    return block * block_bytes + (v ^ base_xor);
}

uint source_byte(uint output) {
    uint elem_bytes = 1u << elem_log2;
    uint element = output >> elem_log2;
    uint in_element = output & (elem_bytes - 1);
    uint plane = width * height;
    uint z = element / plane;
    uint rem = element - z * plane;
    uint y = rem / width;
    uint x = rem - y * width;
    uint at = src_base + source_element(x, y, z) + in_element;
    return (tiled.Load(at & ~3u) >> ((at & 3u) * 8u)) & 0xFFu;
}

[numthreads(64, 1, 1)]
[RootSignature(RS)]
void main(uint3 id : SV_DispatchThreadID) {
    uint first = id.x * 4;
    if (first >= output_bytes) return;
    uint word = 0;
    [unroll] for (uint i = 0; i < 4; ++i) {
        uint at = first + i;
        if (at < output_bytes) word |= source_byte(at) << (i * 8);
    }
    linear.Store(dst_base + first, word);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_hlsl_register_packing() {
        assert_eq!(std::mem::size_of::<Constants>(), 120);
        assert_eq!(std::mem::align_of::<Constants>(), 4);
    }

    #[test]
    fn row_plan_targets_padded_texture_footprints() {
        let layout = super::super::tiling::Layout::new(
            9,
            2,
            super::super::tiling::PipeConfig { pipes_log2: 2, pkrs_log2: 1 },
        ).unwrap().gpu_layout();
        let level = MipLevel { width: 13, height: 3, depth: 2, offset: 0x1000, pitch: 64, origin: (0, 0), tail: None };
        let plan = rows(&layout, &level, 0x2000, 0x4000, 256, 1024, 0x8000, true).unwrap();
        assert_eq!(plan.len(), 6);
        assert_eq!(plan.iter().map(|p| p.destination_base).collect::<Vec<_>>(), [0x4000, 0x4100, 0x4200, 0x4400, 0x4500, 0x4600]);
        assert!(plan.iter().all(|p| p.output_bytes == 13 * 4 && p.height == 1 && p.depth == 1));
        assert_eq!(plan[0].source_base, 0x3000);
        assert_eq!(plan[3].source_base, 0xB000);
    }

    #[test]
    fn row_plan_preserves_volume_layer_and_swizzle_coordinates() {
        let cpu = super::super::tiling::Layout::new_3d(
            5,
            2,
            super::super::tiling::PipeConfig { pipes_log2: 0, pkrs_log2: 0 },
        ).unwrap();
        let gpu = cpu.gpu_layout();
        let level = MipLevel { width: 7, height: 2, depth: gpu.block_d as usize + 1, offset: 0x1000, pitch: 32, origin: (3, 5), tail: None };
        let plan = rows(&gpu, &level, 0x2000, 0, 256, 512, 0x9000, true).unwrap();
        for z in 0..level.depth {
            let constants = &plan[z * level.height];
            let shader_address = constants.source_base as usize
                + gpu.offset(level.origin.0 as u32, level.origin.1 as u32, constants.slice, level.pitch as u32) as usize;
            let cpu_address = 0x2000 + (z / cpu.block_d) * 0x9000 + level.offset
                + cpu.offset_z(level.origin.0, level.origin.1, z, level.pitch);
            assert_eq!(shader_address, cpu_address, "z={z}");
        }
    }

    #[test]
    fn gpu_detile_shader_compiles() {
        if crate::gpu::recomp::dxc::compile(super::HLSL, "main", "cs_6_0", &[]).is_err() {
            crate::gpu::recomp::runner::skip_test("no DXC");
        }
    }
}
