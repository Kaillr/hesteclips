//! Telling a game's new pictures from the same one copied again. The game
//! capture hook gives no signal when the game draws: it copies the game's
//! picture into a texture shared with us, and all we can do is look at that
//! texture now and then (600 times a second for 300 fps blending). A look
//! finds either a new picture or the one it found last time, and frame
//! blending must count each picture once.
//!
//! So each look's picture is kept (a ring of full-size copies) and compared on
//! the GPU, every texel, with the look before it; the result, the largest
//! difference in each of a few thousand blocks, is read back a few
//! milliseconds later. A look that differs brought a new picture, which came
//! up somewhere between the look before and this one: the middle is its time.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::Result;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;

use super::d3d::{Gpu, Latest};
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
    /// When this look, and the one before it, were made.
    at: f64,
    before: f64,
    size: (u32, u32),
    cursor: (i32, i32),
    /// Compared with a look before it (not the first).
    compared: bool,
}

pub(crate) struct Changes {
    ring: Vec<Slot>,
    next: usize,
    /// The slot of the last look, its valid size and when it was made.
    before: Option<(usize, (u32, u32), f64)>,
    blocks: ID3D11RenderTargetView,
    blocks_texture: ID3D11Texture2D,
    pending: VecDeque<Pending>,
    last_cursor: Option<(i32, i32)>,
    /// When the last new picture came up.
    last_new: f64,
    /// How long without a new picture before the cursor moving counts as one.
    quiet: f64,
    /// `HESTECLIPS_HOOK_LOG=1`: looks, new pictures, comparisons dropped unread, since `counted.0`.
    counted: (std::time::Instant, u32, u32, u32),
}

impl Changes {
    /// `look_every`: seconds between looks.
    pub(crate) fn new(gpu: &Gpu, width: u32, height: u32, look_every: f64) -> Result<Self> {
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
            pending: VecDeque::new(),
            last_cursor: None,
            last_new: 0.0,
            // A guess: four looks, two game pictures at the blend rate.
            quiet: look_every * 4.0,
            counted: (std::time::Instant::now(), 0, 0, 0),
        })
    }

    /// Where the next look's picture goes: the slot's number, and what to
    /// draw into.
    pub(crate) fn slot(&mut self) -> (usize, Arc<Latest>, ID3D11RenderTargetView) {
        // Never reuse a slot whose comparison hasn't been read.
        while self.pending.len() >= RING - 1 {
            self.pending.pop_front();
            self.counted.3 += 1;
        }
        let i = self.next;
        (i, self.ring[i].picture.clone(), self.ring[i].target.clone())
    }

    /// The picture drawn into slot `i` (`size` of it valid) is done: compare it
    /// with the look before, to read when it's ready.
    pub(crate) fn submit(&mut self, gpu: &Gpu, quad: &Quad, i: usize, size: (u32, u32), cursor: (i32, i32)) {
        let at = host_now();
        let (compared, before) = match self.before {
            Some((b, before_size, before_at)) if before_size == size => {
                quad.difference(gpu, &self.blocks, BLOCKS, &self.ring[i].view, &self.ring[b].view, size);
                unsafe { gpu.context.CopyResource(&self.ring[i].staging, &self.blocks_texture) };
                (true, before_at)
            }
            _ => (false, at),
        };
        self.pending.push_back(Pending { slot: i, at, before, size, cursor, compared });
        self.counted.1 += 1;
        if self.counted.0.elapsed() >= std::time::Duration::from_secs(5) {
            if std::env::var_os("HESTECLIPS_HOOK_LOG").is_some() {
                let secs = self.counted.0.elapsed().as_secs_f64();
                eprintln!("game hook: {:.0} looks/s, {:.0} new pictures/s, {} comparisons unread", self.counted.1 as f64 / secs, self.counted.2 as f64 / secs, self.counted.3);
            }
            self.counted = (std::time::Instant::now(), 0, 0, 0);
        }
        self.before = Some((i, size, at));
        self.next = (i + 1) % RING;
    }

    /// The comparisons that have arrived: a look that brought a new picture
    /// (or the first) puts it up in `latest`, timed, with the cursor drawn on
    /// it by `draw_cursor` (given where to draw). The same picture with the
    /// cursor moved goes up too, but only counts as a new picture for frame
    /// blending once the game has drawn nothing new for a while (a menu).
    pub(crate) fn resolve(&mut self, gpu: &Gpu, latest: &Latest, latest_target: &ID3D11RenderTargetView, draw_cursor: &mut dyn FnMut(&ID3D11RenderTargetView, (u32, u32))) {
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
                            // Blue, green, red of each block (the alpha is always full): any above 0 is a difference.
                            differs |= line.chunks_exact(4).any(|px| px[..3].iter().any(|&b| b != 0));
                        }
                        gpu.context.Unmap(staging, 0);
                    },
                    Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => return,
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
            // It came up after the look before and by this one.
            let at = (p.before + p.at) / 2.0;
            let counts = differs || at - self.last_new > self.quiet;
            if counts {
                self.last_new = at;
            }
            self.counted.2 += differs as u32;
            unsafe { gpu.context.CopyResource(&latest.texture, &self.ring[p.slot].picture.texture) };
            draw_cursor(latest_target, p.size);
            latest.mark(p.size, at, counts);
            unsafe { gpu.context.Flush() };
        }
    }
}
