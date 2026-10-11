//! Telling a game's new pictures from the same one copied again. The game
//! capture hook gives no signal when the game draws: the capture just copies
//! the shared picture on every look, so at 600 looks a second a game at 300
//! fps seems to send 600 pictures, at times that wobble with the looks.
//!
//! Each look's picture is kept (a ring of full-size copies) and compared, on
//! the GPU, with the look before it, every texel: the result is a small
//! picture (the largest difference in each block), which is read back a few
//! milliseconds later. A look that differs is a new picture; only those go on
//! to the frame blending, with the time they first showed, smoothed (a steady
//! game's pictures come at even steps; the looks' timing is not).

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::Result;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;

use super::d3d::{Gpu, Latest, QUEUE_MAX, Queued};
use super::quad::{self, Quad};
use super::system::host_now;

/// Full-size copies kept while their comparisons are on their way. A guess:
/// at 600 looks a second, 6 are 10 ms.
const RING: usize = 6;
/// The comparison's size, in blocks.
const BLOCKS: (u32, u32) = (160, 90);

struct Slot {
    picture: Arc<Latest>,
    target: ID3D11RenderTargetView,
    view: ID3D11ShaderResourceView,
    /// This look's comparison with the one before, on the CPU's side.
    staging: ID3D11Texture2D,
}

struct Pending {
    slot: usize,
    /// When it was looked at.
    at: f64,
    size: (u32, u32),
    cursor: (i32, i32),
    /// Compared with a look before it (not the first).
    compared: bool,
}

/// Textures for pictures handed on in a queue: the one being shown and the
/// ones waiting, and one to spare.
const POOL: usize = QUEUE_MAX + 3;

struct Pooled {
    picture: Arc<Latest>,
    view: ID3D11ShaderResourceView,
}

pub(crate) struct Changes {
    ring: Vec<Slot>,
    next: usize,
    /// The slot of the last look, and its valid size.
    before: Option<(usize, (u32, u32))>,
    blocks: ID3D11RenderTargetView,
    blocks_texture: ID3D11Texture2D,
    pool: Vec<Pooled>,
    pool_next: usize,
    size: (u32, u32),
    pending: VecDeque<Pending>,
    last_cursor: Option<(i32, i32)>,
    time: Smoother,
}

impl Changes {
    pub(crate) fn new(gpu: &Gpu, width: u32, height: u32) -> Result<Self> {
        let mut ring = Vec::new();
        for _ in 0..RING {
            let picture = Latest::new(gpu, width, height)?;
            ring.push(Slot {
                target: quad::target_view(gpu, &picture.texture)?,
                view: quad::shader_view(gpu, &picture.texture)?,
                staging: gpu.texture(BLOCKS.0, BLOCKS.1, DXGI_FORMAT_B8G8R8A8_UNORM, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32)?,
                picture,
            });
        }
        let blocks_texture = gpu.texture(BLOCKS.0, BLOCKS.1, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET.0 as u32, D3D11_USAGE_DEFAULT, 0)?;
        Ok(Self {
            ring,
            next: 0,
            before: None,
            blocks: quad::target_view(gpu, &blocks_texture)?,
            blocks_texture,
            pool: Vec::new(),
            pool_next: 0,
            size: (width, height),
            pending: VecDeque::new(),
            last_cursor: None,
            time: Smoother::default(),
        })
    }

    /// Where the next look's picture goes: the slot's number, and what to
    /// draw into.
    pub(crate) fn slot(&mut self) -> (usize, Arc<Latest>, ID3D11RenderTargetView) {
        // Never reuse a slot whose comparison hasn't been read.
        while self.pending.len() >= RING - 1 {
            self.pending.pop_front();
        }
        let i = self.next;
        (i, self.ring[i].picture.clone(), self.ring[i].target.clone())
    }

    /// The picture drawn into slot `i` (`size` of it valid) is done: compare it
    /// with the look before, to read when it's ready.
    pub(crate) fn submit(&mut self, gpu: &Gpu, quad: &Quad, i: usize, size: (u32, u32), cursor: (i32, i32)) {
        let compared = match self.before {
            Some((b, before_size)) if before_size == size => {
                quad.difference(gpu, &self.blocks, BLOCKS, &self.ring[i].view, &self.ring[b].view, size);
                unsafe { gpu.context.CopyResource(&self.ring[i].staging, &self.blocks_texture) };
                true
            }
            _ => false,
        };
        self.pending.push_back(Pending { slot: i, at: host_now(), size, cursor, compared });
        self.before = Some((i, size));
        self.next = (i + 1) % RING;
    }

    /// The comparisons that have arrived: a look that differs from the one
    /// before (or the first, or the cursor moved) goes up in `latest`, timed,
    /// with the cursor drawn on it by `draw_cursor` (given where to draw), and
    /// into its queue if something wants every picture.
    pub(crate) fn resolve(
        &mut self,
        gpu: &Gpu,
        latest: &Latest,
        latest_target: &ID3D11RenderTargetView,
        draw_cursor: &mut dyn FnMut(&ID3D11RenderTargetView, (u32, u32)),
    ) -> Result<()> {
        while let Some(p) = self.pending.front() {
            let mut differs = !p.compared;
            if p.compared {
                let staging = &self.ring[p.slot].staging;
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                let read = unsafe { gpu.context.Map(staging, 0, D3D11_MAP_READ, D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32, Some(&mut mapped)) };
                match read {
                    Ok(()) => unsafe {
                        let row_bytes = BLOCKS.0 as usize * 4;
                        for row in 0..BLOCKS.1 as usize {
                            let line = std::slice::from_raw_parts((mapped.pData as *const u8).add(row * mapped.RowPitch as usize), row_bytes);
                            // Blue, green, red of each block: any above 0 is a difference.
                            differs |= line.iter().any(|&b| b != 0);
                        }
                        gpu.context.Unmap(staging, 0);
                    },
                    Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => return Ok(()),
                    Err(_) => {
                        self.pending.pop_front();
                        continue;
                    }
                }
            }
            let p = self.pending.pop_front().unwrap();
            let moved = self.last_cursor != Some(p.cursor);
            if !(differs || moved) {
                continue;
            }
            self.last_cursor = Some(p.cursor);
            // A picture that's new has a time of its own; the same one with
            // the cursor moved just shows now.
            let at = if differs { self.time.smooth(p.at) } else { p.at.max(self.time.last.unwrap_or(0.0) + 1e-6).min(host_now()) };
            unsafe { gpu.context.CopyResource(&latest.texture, &self.ring[p.slot].picture.texture) };
            draw_cursor(latest_target, p.size);
            if latest.wants_queue() {
                if self.pool.is_empty() {
                    for _ in 0..POOL {
                        let picture = Latest::new(gpu, self.size.0, self.size.1)?;
                        self.pool.push(Pooled { view: quad::shader_view(gpu, &picture.texture)?, picture });
                    }
                }
                let slot = &self.pool[self.pool_next];
                self.pool_next = (self.pool_next + 1) % POOL;
                unsafe { gpu.context.CopyResource(&slot.picture.texture, &latest.texture) };
                latest.push_queued(Queued { texture: slot.picture.texture.clone(), view: slot.view.clone(), size: p.size, at });
            }
            unsafe { gpu.context.Flush() };
            latest.mark_frame_at(p.size, at);
        }
        Ok(())
    }
}

/// A game's pictures come at even steps; the looks that notice them don't.
/// Follows the pace so each picture's time is where the pace puts it, unless
/// it's far off (a stall, a hitch), then the time is taken as it is.
#[derive(Default)]
struct Smoother {
    last: Option<f64>,
    step: Option<f64>,
}

impl Smoother {
    fn smooth(&mut self, seen: f64) -> f64 {
        let t = match (self.last, self.step) {
            (Some(last), Some(step)) => {
                let expected = last + step;
                if (seen - expected).abs() < step * 0.4 {
                    // A guess: follow the looks 15% of the way.
                    expected + (seen - expected) * 0.15
                } else {
                    seen
                }
            }
            _ => seen,
        };
        if let Some(last) = self.last {
            let gap = t - last;
            self.step = if gap > 0.0 && gap < 0.1 { Some(self.step.map_or(gap, |s| s * 0.9 + gap * 0.1)) } else { None };
        }
        // Never before the last.
        let t = t.max(self.last.map_or(t, |l| l + 1e-6));
        self.last = Some(t);
        t
    }
}
