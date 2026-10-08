//! Drawing one texture onto another with the GPU: a rectangle of it, placed
//! anywhere, optionally blended over what's there. For the game capture
//! hook's pictures (any format, upside down from OpenGL, shrunk to fit) and
//! the cursor drawn over them.

use anyhow::{Context, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, D3D11_SRV_DIMENSION_TEXTURE2D, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{PCSTR, s};

use super::d3d::Gpu;

const SHADER: &str = r"
cbuffer Place : register(b0) { float4 rect; float4 uv_rect; };
Texture2D image : register(t0);
SamplerState linear_clamp : register(s0);
struct V { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
V vs(uint id : SV_VertexID) {
    float2 corner = float2(id & 1, id >> 1);
    V o;
    o.pos = float4(lerp(rect.x, rect.z, corner.x), lerp(rect.y, rect.w, corner.y), 0, 1);
    o.uv = float2(lerp(uv_rect.x, uv_rect.z, corner.x), lerp(uv_rect.y, uv_rect.w, corner.y));
    return o;
}
float4 ps(V i) : SV_Target { return image.Sample(linear_clamp, i.uv); }
float4 ps_opaque(V i) : SV_Target { return float4(image.Sample(linear_clamp, i.uv).rgb, 1); }
";

/// The shaders and states for drawing textured rectangles.
pub(crate) struct Quad {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    ps_opaque: ID3D11PixelShader,
    blend: ID3D11BlendState,
    sampler: ID3D11SamplerState,
    place: ID3D11Buffer,
}
unsafe impl Send for Quad {}

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    let result = unsafe { D3DCompile(SHADER.as_ptr().cast(), SHADER.len(), s!("quad"), None, None, entry, target, 0, 0, &mut code, Some(&mut errors)) };
    if let Err(e) = result {
        let msg = errors.map(|b: ID3DBlob| String::from_utf8_lossy(bytes(&b)).into_owned()).unwrap_or_default();
        anyhow::bail!("shader: {e} {msg}");
    }
    code.context("no shader code")
}

fn bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()) }
}

/// Where a texture goes and which part of it, in pixels.
pub(crate) struct Placement {
    /// In the target: left, top, right, bottom.
    pub dest: [f32; 4],
    /// Of the source, as fractions of it (0–1): left, top, right, bottom.
    /// Top and bottom swapped turns it upside down.
    pub uv: [f32; 4],
    /// Only this part of the target is drawn on.
    pub clip: RECT,
}

impl Quad {
    pub(crate) fn new(gpu: &Gpu) -> Result<Self> {
        let dev = &gpu.device;
        let vs_code = compile(s!("vs"), s!("vs_4_0"))?;
        let ps_code = compile(s!("ps"), s!("ps_4_0"))?;
        let ps_opaque_code = compile(s!("ps_opaque"), s!("ps_4_0"))?;
        unsafe {
            let mut vs = None;
            dev.CreateVertexShader(bytes(&vs_code), None, Some(&mut vs))?;
            let mut ps = None;
            dev.CreatePixelShader(bytes(&ps_code), None, Some(&mut ps))?;
            let mut ps_opaque = None;
            dev.CreatePixelShader(bytes(&ps_opaque_code), None, Some(&mut ps_opaque))?;
            // Straight alpha over the picture; the picture stays opaque.
            let mut blend_desc = D3D11_BLEND_DESC::default();
            blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_SRC_ALPHA,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ZERO,
                DestBlendAlpha: D3D11_BLEND_ONE,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut blend = None;
            dev.CreateBlendState(&blend_desc, Some(&mut blend))?;
            let sampler_desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            dev.CreateSamplerState(&sampler_desc, Some(&mut sampler))?;
            let buffer_desc = D3D11_BUFFER_DESC { ByteWidth: 32, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            let mut place = None;
            dev.CreateBuffer(&buffer_desc, None, Some(&mut place))?;
            Ok(Self {
                vs: vs.context("no vertex shader")?,
                ps: ps.context("no pixel shader")?,
                ps_opaque: ps_opaque.context("no pixel shader")?,
                blend: blend.context("no blend state")?,
                sampler: sampler.context("no sampler")?,
                place: place.context("no constant buffer")?,
            })
        }
    }

    /// Draw `source` into `target` (`size` pixels) where `at` says: blended
    /// by its alpha, or (`opaque`) replacing what's there.
    pub(crate) fn draw(&self, gpu: &Gpu, target: &ID3D11RenderTargetView, size: (u32, u32), source: &ID3D11ShaderResourceView, at: &Placement, opaque: bool) {
        let (w, h) = (size.0 as f32, size.1 as f32);
        let [l, t, r, b] = at.dest;
        let place = [l / w * 2.0 - 1.0, 1.0 - t / h * 2.0, r / w * 2.0 - 1.0, 1.0 - b / h * 2.0, at.uv[0], at.uv[1], at.uv[2], at.uv[3]];
        let ctx = &gpu.context;
        unsafe {
            ctx.UpdateSubresource(&self.place, 0, None, place.as_ptr().cast(), 0, 0);
            ctx.RSSetScissorRects(Some(&[at.clip]));
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: w, Height: h, MinDepth: 0.0, MaxDepth: 1.0 }]));
            ctx.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            ctx.OMSetBlendState(if opaque { None } else { Some(&self.blend) }, None, 0xffff_ffff);
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.VSSetShader(&self.vs, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.place.clone())]));
            ctx.PSSetShader(if opaque { &self.ps_opaque } else { &self.ps }, None);
            ctx.PSSetShaderResources(0, Some(&[Some(source.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.Draw(4, 0);
            // Leave nothing bound that others might trip over.
            ctx.OMSetRenderTargets(None, None);
            ctx.PSSetShaderResources(0, Some(&[None]));
        }
    }
}

/// A view to sample a texture through (a typeless one read as plain colour).
pub(crate) fn shader_view(gpu: &Gpu, texture: &ID3D11Texture2D) -> Result<ID3D11ShaderResourceView> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    let format = match desc.Format {
        DXGI_FORMAT_B8G8R8A8_TYPELESS => DXGI_FORMAT_B8G8R8A8_UNORM,
        DXGI_FORMAT_R8G8B8A8_TYPELESS => DXGI_FORMAT_R8G8B8A8_UNORM,
        DXGI_FORMAT_B8G8R8X8_TYPELESS => DXGI_FORMAT_B8G8R8X8_UNORM,
        DXGI_FORMAT_R10G10B10A2_TYPELESS => DXGI_FORMAT_R10G10B10A2_UNORM,
        DXGI_FORMAT_R16G16B16A16_TYPELESS => DXGI_FORMAT_R16G16B16A16_FLOAT,
        f => f,
    };
    let view_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 } },
    };
    let mut view = None;
    unsafe { gpu.device.CreateShaderResourceView(texture, Some(&view_desc), Some(&mut view)) }.context("can't read the picture")?;
    view.context("no shader view")
}

/// A view to draw into a texture.
pub(crate) fn target_view(gpu: &Gpu, texture: &ID3D11Texture2D) -> Result<ID3D11RenderTargetView> {
    let mut view = None;
    unsafe { gpu.device.CreateRenderTargetView(texture, None, Some(&mut view)) }?;
    view.context("no render target view")
}
