//! The mouse cursor, drawn over a game's picture from the capture hook (which,
//! unlike Windows Graphics Capture, copies the game's frames without it):
//! Windows' current cursor image, where the cursor is in the game's window,
//! while the game is in focus and shows a cursor.

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, ClientToScreen, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW, HBITMAP, HGDIOBJ, ReleaseDC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, GetClientRect, GetCursorInfo, GetForegroundWindow, GetIconInfo, GetWindowThreadProcessId, HICON, ICONINFO,
};

use super::d3d::{Gpu, Latest};
use super::quad::{Placement, Quad};

/// Draws the cursor into a [`Latest`].
pub(crate) struct CursorLayer {
    /// The cursor image uploaded, by Windows' handle for it.
    image: Option<(isize, Image)>,
}

struct Image {
    view: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
    hotspot: (u32, u32),
}

impl CursorLayer {
    pub(crate) fn new() -> Self {
        Self { image: None }
    }

    /// Draw the cursor over the game's picture in `latest` (through `target`;
    /// `size` of it, from the top-left corner, showing `window`'s client
    /// area), if the game (process `pid`) is in focus and Windows shows a
    /// cursor.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw(&mut self, gpu: &Gpu, quad: &Quad, latest: &Latest, target: &ID3D11RenderTargetView, window: HWND, pid: u32, size: (u32, u32)) {
        if let Err(e) = self.try_draw(gpu, quad, latest, target, window, pid, size) {
            eprintln!("cursor: {e:#}");
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn try_draw(&mut self, gpu: &Gpu, quad: &Quad, latest: &Latest, target: &ID3D11RenderTargetView, window: HWND, pid: u32, size: (u32, u32)) -> Result<()> {
        let mut focused = 0u32;
        unsafe { GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut focused)) };
        if focused != pid {
            return Ok(());
        }
        let mut info = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
        unsafe { GetCursorInfo(&mut info) }?;
        if info.flags.0 & CURSOR_SHOWING.0 == 0 || info.hCursor.is_invalid() {
            return Ok(());
        }
        // Where the client area is and how big, in physical pixels (this
        // thread is per-monitor DPI aware), against the picture's size: a game
        // can draw at another resolution than its window.
        let mut client = RECT::default();
        let mut origin = POINT::default();
        unsafe {
            GetClientRect(window, &mut client)?;
            let _ = ClientToScreen(window, &mut origin);
        }
        let (cw, ch) = ((client.right - client.left).max(1) as f32, (client.bottom - client.top).max(1) as f32);
        let (sx, sy) = (size.0 as f32 / cw, size.1 as f32 / ch);
        let handle = info.hCursor.0 as isize;
        if self.image.as_ref().is_none_or(|(h, _)| *h != handle) {
            self.image = cursor_image(info.hCursor.0).map(|img| upload(gpu, img)).transpose()?.map(|i| (handle, i));
        }
        let Some((_, image)) = &self.image else { return Ok(()) };
        let x = (info.ptScreenPos.x - origin.x - image.hotspot.0 as i32) as f32 * sx;
        let y = (info.ptScreenPos.y - origin.y - image.hotspot.1 as i32) as f32 * sy;
        let (w, h) = (image.width as f32 * sx, image.height as f32 * sy);
        if x + w <= 0.0 || y + h <= 0.0 || x >= size.0 as f32 || y >= size.1 as f32 {
            return Ok(());
        }
        let at = Placement {
            dest: [x, y, x + w, y + h],
            uv: [0.0, 0.0, 1.0, 1.0],
            // Only on the picture.
            clip: RECT { left: 0, top: 0, right: size.0 as i32, bottom: size.1 as i32 },
        };
        quad.draw(gpu, target, (latest.width, latest.height), &image.view, &at, false);
        Ok(())
    }
}

/// A cursor's picture: BGRA rows (straight alpha), its size, and its hotspot
/// (the pixel that points).
type CursorPicture = (Vec<u8>, u32, u32, (u32, u32));

fn upload(gpu: &Gpu, (bgra, width, height, hotspot): CursorPicture) -> Result<Image> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let data = D3D11_SUBRESOURCE_DATA { pSysMem: bgra.as_ptr().cast(), SysMemPitch: width * 4, SysMemSlicePitch: 0 };
    unsafe {
        let mut tex = None;
        gpu.device.CreateTexture2D(&desc, Some(&data), Some(&mut tex))?;
        let tex = tex.context("no cursor texture")?;
        let mut view = None;
        gpu.device.CreateShaderResourceView(&tex, None, Some(&mut view))?;
        Ok(Image { view: view.context("no cursor view")?, width, height, hotspot })
    }
}

/// A cursor's picture, by Windows' handle for it.
fn cursor_image(cursor: *mut core::ffi::c_void) -> Option<CursorPicture> {
    let mut info = ICONINFO::default();
    unsafe { GetIconInfo(HICON(cursor), &mut info) }.ok()?;
    let hotspot = (info.xHotspot, info.yHotspot);
    let mask = bitmap_bgra(info.hbmMask);
    let color = (!info.hbmColor.is_invalid()).then(|| bitmap_bgra(info.hbmColor)).flatten();
    unsafe {
        let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        }
    }
    let (mask, mw, mh) = mask?;
    match color {
        // A colour cursor: its own alpha, or (old ones without any) the mask's.
        Some((mut px, w, h)) => {
            if px.chunks_exact(4).all(|p| p[3] == 0) {
                for (i, p) in px.chunks_exact_mut(4).enumerate() {
                    let transparent = mask.get(i * 4).is_some_and(|&m| m != 0);
                    p[3] = if transparent { 0 } else { 255 };
                }
            }
            Some((px, w, h, hotspot))
        }
        // Black and white: the mask's top half says where it's see-through,
        // the bottom half black or white. See-through and white means
        // "invert the screen", drawn black.
        None => {
            let h = mh / 2;
            let mut px = vec![0u8; (mw * h * 4) as usize];
            for i in 0..(mw * h) as usize {
                let and = mask[i * 4] != 0;
                let xor = mask[(i + (mw * h) as usize) * 4] != 0;
                let (v, a) = match (and, xor) {
                    (true, false) => (0, 0),
                    (false, x) => (if x { 255 } else { 0 }, 255),
                    (true, true) => (0, 255),
                };
                px[i * 4..i * 4 + 4].copy_from_slice(&[v, v, v, a]);
            }
            Some((px, mw, h, hotspot))
        }
    }
}

/// A bitmap's pixels as 32-bit BGRA rows, top to bottom (black and white ones
/// come out black and white), and its size.
fn bitmap_bgra(bitmap: HBITMAP) -> Option<(Vec<u8>, u32, u32)> {
    let mut bm = BITMAP::default();
    let got = unsafe { GetObjectW(HGDIOBJ(bitmap.0), std::mem::size_of::<BITMAP>() as i32, Some((&mut bm as *mut BITMAP).cast())) };
    if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
        return None;
    }
    let (w, h) = (bm.bmWidth as u32, bm.bmHeight as u32);
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut px = vec![0u8; (w * h * 4) as usize];
    unsafe {
        let dc = GetDC(None);
        let lines = GetDIBits(dc, bitmap, 0, h, Some(px.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        (lines == h as i32).then_some((px, w, h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{IDC_ARROW, IDC_IBEAM, LoadCursorW};

    #[test]
    fn standard_cursors_read() {
        for id in [IDC_ARROW, IDC_IBEAM] {
            let cursor = unsafe { LoadCursorW(None, id) }.unwrap();
            let (px, w, h, hotspot) = cursor_image(cursor.0).expect("cursor picture");
            assert!(w >= 16 && h >= 16, "{w}×{h}");
            assert_eq!(px.len(), (w * h * 4) as usize);
            let opaque = px.chunks_exact(4).filter(|p| p[3] == 255).count();
            let clear = px.chunks_exact(4).filter(|p| p[3] == 0).count();
            assert!(opaque > 10 && clear > 10, "{opaque} opaque, {clear} clear");
            assert!(hotspot.0 < w && hotspot.1 < h);
        }
    }
}
