//! The one HLSL program used by every renderer, compiled at runtime with
//! `D3DCompile` (d3dcompiler_47.dll ships with Windows 8.1+).

use crate::error::{Error, Result};
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCOMPILE_ENABLE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile,
};
use windows::Win32::Graphics::Direct3D::{D3D_SHADER_MACRO, ID3DBlob};
use windows::core::{PCSTR, s};

pub(crate) const SOURCE: &str = r#"
cbuffer Consts : register(b0) {
    float2 scale;
    uint   srgb;
    uint   nearest;
};

struct VSIn  { float2 pos : POSITION; float2 uv : TEXCOORD0; float4 col : COLOR0; };
struct PSIn  { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; float4 col : COLOR0; };

PSIn vs_main(VSIn i) {
    PSIn o;
    o.pos = float4(i.pos.x * scale.x - 1.0, 1.0 - i.pos.y * scale.y, 0.0, 1.0);
    o.uv  = i.uv;
    o.col = i.col;
    return o;
}

Texture2D    tex      : register(t0);
SamplerState s_linear : register(s0);
#ifdef OVERHOOK_DX12
SamplerState s_point  : register(s1);
#endif

float3 to_linear(float3 c) {
    return c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}

float4 ps_main(PSIn i) : SV_Target {
#ifdef OVERHOOK_DX12
    float4 t = nearest != 0 ? tex.Sample(s_point, i.uv) : tex.Sample(s_linear, i.uv);
#else
    float4 t = tex.Sample(s_linear, i.uv);
#endif
    float4 c = i.col * t;
    if (srgb != 0) {
        c.rgb = to_linear(c.rgb);
    }
    return c;
}
"#;

pub(crate) struct Blobs {
    pub vs: ID3DBlob,
    pub ps: ID3DBlob,
}

pub(crate) fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }
}

fn compile(entry: PCSTR, target: PCSTR, dx12: bool) -> Result<ID3DBlob> {
    let defines = [
        D3D_SHADER_MACRO {
            Name: s!("OVERHOOK_DX12"),
            Definition: s!("1"),
        },
        D3D_SHADER_MACRO::default(),
    ];
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let r = unsafe {
        D3DCompile(
            SOURCE.as_ptr() as *const _,
            SOURCE.len(),
            s!("overhook.hlsl"),
            if dx12 { Some(defines.as_ptr()) } else { None },
            None,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3 | D3DCOMPILE_ENABLE_STRICTNESS,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = r {
        let msg = errors
            .as_ref()
            .map(|b| String::from_utf8_lossy(blob_bytes(b)).into_owned())
            .unwrap_or_else(|| e.to_string());
        return Err(Error::Shader(msg));
    }
    code.ok_or_else(|| Error::Shader("D3DCompile returned no code".into()))
}

pub(crate) fn compile_all(dx12: bool) -> Result<Blobs> {
    Ok(Blobs {
        vs: compile(s!("vs_main"), s!("vs_5_0"), dx12)?,
        ps: compile(s!("ps_main"), s!("ps_5_0"), dx12)?,
    })
}
