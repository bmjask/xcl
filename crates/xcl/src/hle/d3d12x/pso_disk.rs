//! Driver pipeline blobs (`ID3D12PipelineState::GetCachedBlob`).
//!
//! Translating a console shader is cached as DXIL. The driver then compiles that DXIL again
//! on every launch. A blob from `GetCachedBlob` is that compile: the next
//! `Create*PipelineState` passes it in `CachedPSO` and the driver skips the work when the
//! blob still matches this device. A driver update rejects the blob; the caller drops the
//! file and compiles again.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;

const MAGIC: &[u8; 4] = b"XPSO";
const VERSION: u32 = 1;

fn dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        if let Ok(d) = std::env::var("XCL_PSO_CACHE") {
            return PathBuf::from(d);
        }
        let base = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
        base.join("xcl").join("pso")
    })
    .clone()
}

fn memory() -> &'static Mutex<std::collections::HashMap<u64, std::sync::Arc<Vec<u8>>>> {
    static M: OnceLock<Mutex<std::collections::HashMap<u64, std::sync::Arc<Vec<u8>>>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn path_for(dir: &Path, key: u64) -> PathBuf {
    dir.join(format!("{key:016x}.blob"))
}

fn payload(file: &[u8]) -> Option<&[u8]> {
    if file.len() < 8 || &file[..4] != MAGIC || u32::from_le_bytes(file[4..8].try_into().ok()?) != VERSION {
        return None;
    }
    let blob = &file[8..];
    (!blob.is_empty() && blob.len() <= 32 << 20).then_some(blob)
}

/// A blob saved for `key`, from this process or from disk.
pub fn load(key: u64) -> Option<std::sync::Arc<Vec<u8>>> {
    if let Some(hit) = memory().lock().unwrap().get(&key).cloned() {
        return Some(hit);
    }
    let bytes = std::fs::read(path_for(&dir(), key)).ok()?;
    let blob = payload(&bytes)?.to_vec();
    let blob = std::sync::Arc::new(blob);
    memory().lock().unwrap().insert(key, blob.clone());
    Some(blob)
}

/// Write `blob` so the next launch can reuse this driver's compile.
pub fn store(key: u64, blob: &[u8]) {
    if blob.is_empty() || blob.len() > 32 << 20 {
        return;
    }
    let owned = std::sync::Arc::new(blob.to_vec());
    memory().lock().unwrap().insert(key, owned);
    let dir = dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = path_for(&dir, key);
    if std::fs::read(&path).ok().and_then(|b| payload(&b).map(|p| p == blob)).unwrap_or(false) {
        return;
    }
    let mut body = Vec::with_capacity(8 + blob.len());
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&VERSION.to_le_bytes());
    body.extend_from_slice(blob);
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, &body).is_ok() {
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// The driver refused this blob (a new driver, or a different pipeline). Do not try it again.
pub fn forget(key: u64) {
    memory().lock().unwrap().remove(&key);
    let _ = std::fs::remove_file(path_for(&dir(), key));
}

fn mix(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
}

fn mix_u32(h: &mut u64, v: u32) {
    mix(h, &v.to_le_bytes());
}

fn mix_shader(h: &mut u64, s: &D3D12_SHADER_BYTECODE) {
    let n = s.BytecodeLength;
    let p = s.pShaderBytecode as usize;
    if n == 0 || !crate::gpu::shader::readable(p, n) {
        mix_u32(h, 0);
        return;
    }
    mix_u32(h, n as u32);
    mix(h, unsafe { std::slice::from_raw_parts(p as *const u8, n) });
}

fn mix_input(h: &mut u64, layout: &D3D12_INPUT_LAYOUT_DESC) {
    let n = layout.NumElements as usize;
    mix_u32(h, layout.NumElements);
    if n == 0 || n > 32 || layout.pInputElementDescs.is_null() {
        return;
    }
    let bytes = n * std::mem::size_of::<D3D12_INPUT_ELEMENT_DESC>();
    if !crate::gpu::shader::readable(layout.pInputElementDescs as usize, bytes) {
        return;
    }
    let elems = unsafe { std::slice::from_raw_parts(layout.pInputElementDescs, n) };
    for e in elems {
        if !e.SemanticName.is_null() {
            let mut k = 0usize;
            while k < 64 {
                let c = unsafe { *e.SemanticName.0.add(k) };
                mix(h, &[c]);
                if c == 0 {
                    break;
                }
                k += 1;
            }
        }
        mix_u32(h, e.SemanticIndex);
        mix_u32(h, e.Format.0 as u32);
        mix_u32(h, e.InputSlot);
        mix_u32(h, e.AlignedByteOffset);
        mix_u32(h, e.InputSlotClass.0 as u32);
        mix_u32(h, e.InstanceDataStepRate);
    }
}

fn mix_blend(h: &mut u64, b: &D3D12_BLEND_DESC) {
    mix_u32(h, b.AlphaToCoverageEnable.0 as u32);
    mix_u32(h, b.IndependentBlendEnable.0 as u32);
    for rt in &b.RenderTarget {
        mix_u32(h, rt.BlendEnable.0 as u32);
        mix_u32(h, rt.LogicOpEnable.0 as u32);
        mix_u32(h, rt.SrcBlend.0 as u32);
        mix_u32(h, rt.DestBlend.0 as u32);
        mix_u32(h, rt.BlendOp.0 as u32);
        mix_u32(h, rt.SrcBlendAlpha.0 as u32);
        mix_u32(h, rt.DestBlendAlpha.0 as u32);
        mix_u32(h, rt.BlendOpAlpha.0 as u32);
        mix_u32(h, rt.LogicOp.0 as u32);
        mix_u32(h, rt.RenderTargetWriteMask as u32);
    }
}

fn mix_op(h: &mut u64, o: &D3D12_DEPTH_STENCILOP_DESC) {
    mix_u32(h, o.StencilFailOp.0 as u32);
    mix_u32(h, o.StencilDepthFailOp.0 as u32);
    mix_u32(h, o.StencilPassOp.0 as u32);
    mix_u32(h, o.StencilFunc.0 as u32);
}

fn mix_depth(h: &mut u64, z: &D3D12_DEPTH_STENCIL_DESC) {
    mix_u32(h, z.DepthEnable.0 as u32);
    mix_u32(h, z.DepthWriteMask.0 as u32);
    mix_u32(h, z.DepthFunc.0 as u32);
    mix_u32(h, z.StencilEnable.0 as u32);
    mix(h, &[z.StencilReadMask, z.StencilWriteMask]);
    mix_op(h, &z.FrontFace);
    mix_op(h, &z.BackFace);
}

fn mix_raster(h: &mut u64, r: &D3D12_RASTERIZER_DESC) {
    mix_u32(h, r.FillMode.0 as u32);
    mix_u32(h, r.CullMode.0 as u32);
    mix(h, &r.FrontCounterClockwise.0.to_le_bytes());
    mix(h, &r.DepthBias.to_le_bytes());
    mix(h, &r.DepthBiasClamp.to_le_bytes());
    mix(h, &r.SlopeScaledDepthBias.to_le_bytes());
    mix_u32(h, r.DepthClipEnable.0 as u32);
    mix_u32(h, r.MultisampleEnable.0 as u32);
    mix_u32(h, r.AntialiasedLineEnable.0 as u32);
    mix_u32(h, r.ForcedSampleCount);
    mix_u32(h, r.ConservativeRaster.0 as u32);
}

fn mix_samplers(h: &mut u64, samplers: &[([u32; 4], bool)]) {
    mix_u32(h, samplers.len() as u32);
    for (words, comparison) in samplers {
        for w in words {
            mix_u32(h, *w);
        }
        mix(h, &[*comparison as u8]);
    }
}

/// Identity of a graphics pipeline the driver compiled. The root signature pointer is not
/// part of it; `samplers` is, because that is what the XCL root signature was built from.
pub fn graphics_key(d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC, samplers: &[([u32; 4], bool)]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    mix(&mut h, b"gfx1");
    mix_shader(&mut h, &d.VS);
    mix_shader(&mut h, &d.PS);
    mix_shader(&mut h, &d.DS);
    mix_shader(&mut h, &d.HS);
    mix_shader(&mut h, &d.GS);
    mix_blend(&mut h, &d.BlendState);
    mix_u32(&mut h, d.SampleMask);
    mix_raster(&mut h, &d.RasterizerState);
    mix_depth(&mut h, &d.DepthStencilState);
    mix_input(&mut h, &d.InputLayout);
    mix_u32(&mut h, d.IBStripCutValue.0 as u32);
    mix_u32(&mut h, d.PrimitiveTopologyType.0 as u32);
    mix_u32(&mut h, d.NumRenderTargets);
    for f in &d.RTVFormats {
        mix_u32(&mut h, f.0 as u32);
    }
    mix_u32(&mut h, d.DSVFormat.0 as u32);
    mix_u32(&mut h, d.SampleDesc.Count);
    mix_u32(&mut h, d.SampleDesc.Quality);
    mix_u32(&mut h, d.NodeMask);
    mix_u32(&mut h, d.Flags.0 as u32);
    mix_samplers(&mut h, samplers);
    h
}

pub fn compute_key(d: &D3D12_COMPUTE_PIPELINE_STATE_DESC, samplers: &[([u32; 4], bool)]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    mix(&mut h, b"cs1");
    mix_shader(&mut h, &d.CS);
    mix_u32(&mut h, d.NodeMask);
    mix_u32(&mut h, d.Flags.0 as u32);
    mix_samplers(&mut h, samplers);
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_bytes_change_the_key_and_the_blob_round_trips() {
        let vs = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut g = D3D12_GRAPHICS_PIPELINE_STATE_DESC::default();
        g.VS = D3D12_SHADER_BYTECODE { pShaderBytecode: vs.as_ptr() as *const _, BytecodeLength: vs.len() };
        g.NumRenderTargets = 1;
        g.RTVFormats[0] = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;
        let k1 = graphics_key(&g, &[]);
        let vs2 = [1u8, 2, 3, 4, 5, 6, 7, 9];
        g.VS.pShaderBytecode = vs2.as_ptr() as *const _;
        assert_ne!(k1, graphics_key(&g, &[]));
        g.VS.pShaderBytecode = vs.as_ptr() as *const _;
        assert_eq!(k1, graphics_key(&g, &[]));
        assert_ne!(k1, graphics_key(&g, &[([1, 2, 3, 4], false)]));

        let dir = std::env::temp_dir().join(format!("xcl-pso-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = path_for(&dir, k1);
        let blob = b"driver-blob-bytes";
        let mut body = Vec::new();
        body.extend_from_slice(MAGIC);
        body.extend_from_slice(&VERSION.to_le_bytes());
        body.extend_from_slice(blob);
        std::fs::write(&file, &body).unwrap();
        let read = std::fs::read(&file).unwrap();
        assert_eq!(payload(&read), Some(blob.as_slice()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
