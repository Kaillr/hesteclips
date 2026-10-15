//! Telling a game's new pictures from the same one copied again. The game
//! capture hook gives no signal when the game draws: it copies the game's
//! picture into a texture shared with us, and all we can do is look at that
//! texture now and then (600 times a second for 300 fps blending). A look
//! finds either a new picture or the one it found last time, and frame
//! blending must count each picture once.
//!
//! So each look's picture is kept (a ring of full-size copies) and compared on
//! the GPU with the look before it; the result is read back a few
//! milliseconds later. A look that differs brought a new picture, which came
//! up somewhere between the look before and this one: the middle is its time.
//!
//! Where the game's texture can be read by a shader ([`Changes::read`]), each
//! look makes only a small signature of it (sums over blocks of every other
//! row), compares that with the last look's, and the GPU copies the picture
//! only if they differ (a predicated copy): the cost follows the game's new
//! pictures, not the looks, so it can look several times per game picture
//! (when exactly each came up decides which frame it's in). Copying and
//! comparing the whole picture on every look took 30% of an RTX 4060 Ti for a
//! 1440p game looked at 600 times a second; looking twice as often, 49%. A
//! change the signature can't see (within the rows it skips) shows within
//! [`FORCE_AFTER`]. Otherwise ([`Changes::submit`]) every look is copied and
//! compared texel by texel.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{Context, Result};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;

use super::d3d::{Gpu, Latest};
use super::quad::{self, Quad};
use super::system::host_now;

/// Looks kept while their comparisons are on their way (full-size picture
/// slots, filled only when a look brought a new picture). A guess: at 1200
/// looks a second, 12 are 10 ms.
const RING: usize = 12;
/// With no change seen for this long, the next look copies the picture
/// anyway (a change the signature missed goes up then). A guess.
const FORCE_AFTER: f64 = 0.1;
/// The signature reads every this many rows.
const SIGNATURE_ROWS: u32 = 2;
/// The comparison's size, in blocks.
const BLOCKS: (u32, u32) = (160, 90);

struct Slot {
    picture: Arc<Latest>,
    target: ID3D11RenderTargetView,
    view: ID3D11ShaderResourceView,
    /// This look's comparison with the one before, on the CPU's side.
    staging: ID3D11Texture2D,
    /// This look's signature, and whether it differs from the one before.
    signature: (ID3D11RenderTargetView, ID3D11ShaderResourceView),
    predicate: ID3D11Predicate,
}

/// How a look is known to have brought a new picture.
enum Check {
    /// The first look: it has.
    First,
    /// Copied without comparing (see `FORCE_AFTER`): may have.
    Forced,
    /// Compared texel by texel; read back from `staging`.
    Full,
    /// Its signature compared; the predicate says, and the picture was copied
    /// only if it did.
    Predicate,
}

struct Pending {
    slot: usize,
    /// When this look, and the one before it, were made.
    at: f64,
    before: f64,
    size: (u32, u32),
    cursor: (i32, i32),
    check: Check,
}

pub(crate) struct Changes {
    ring: Vec<Slot>,
    next: usize,
    /// The slot of the last look, its valid size and when it was made.
    before: Option<(usize, (u32, u32), f64)>,
    blocks: ID3D11RenderTargetView,
    blocks_texture: ID3D11Texture2D,
    /// The slot holding the newest picture copied, of those resolved (a look
    /// that brought none left its slot as it was).
    copied: Option<usize>,
    /// When a look last copied without comparing.
    last_forced: f64,
    pending: VecDeque<Pending>,
    last_cursor: Option<(i32, i32)>,
    /// When the last new picture came up.
    last_new: f64,
    /// How long without a new picture before the cursor moving counts as one.
    quiet: f64,
    /// `HASTECLIP_HOOK_LOG=1`: looks, new pictures, comparisons dropped unread, since `counted.0`.
    counted: (std::time::Instant, u32, u32, u32),
}

impl Changes {
    /// `look_every`: seconds between looks.
    pub(crate) fn new(gpu: &Gpu, width: u32, height: u32, look_every: f64) -> Result<Self> {
        let mut ring = Vec::new();
        let float = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32G32B32A32_FLOAT;
        let bind = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
        for _ in 0..RING {
            let picture = Latest::new(gpu, width, height)?;
            let signature = gpu.texture(BLOCKS.0, BLOCKS.1, float, bind, D3D11_USAGE_DEFAULT, 0)?;
            let mut predicate = None;
            unsafe { gpu.device.CreatePredicate(&D3D11_QUERY_DESC { Query: D3D11_QUERY_OCCLUSION_PREDICATE, MiscFlags: 0 }, Some(&mut predicate))? };
            ring.push(Slot {
                target: quad::target_view(gpu, &picture.texture)?,
                view: quad::shader_view(gpu, &picture.texture)?,
                staging: gpu.texture(BLOCKS.0, BLOCKS.1, DXGI_FORMAT_B8G8R8A8_UNORM, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32)?,
                signature: (quad::target_view(gpu, &signature)?, quad::shader_view(gpu, &signature)?),
                predicate: predicate.context("no predicate")?,
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
            copied: None,
            last_forced: 0.0,
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
        let (check, before) = match self.before {
            Some((b, before_size, before_at)) if before_size == size => {
                quad.difference(gpu, &self.blocks, BLOCKS, &self.ring[i].view, &self.ring[b].view, size);
                unsafe { gpu.context.CopyResource(&self.ring[i].staging, &self.blocks_texture) };
                (Check::Full, before_at)
            }
            _ => (Check::First, at),
        };
        self.push(Pending { slot: i, at, before, size, cursor, check }, size);
    }

    /// A look at the game's picture `source` (`source_size` of it valid): its
    /// signature into slot `i` and, if that differs from the last look's,
    /// `copy` (which draws the picture into the slot and says its size there)
    /// on the GPU. `copy` is always called; the GPU skips what it draws when
    /// nothing changed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn read(&mut self, gpu: &Gpu, quad: &Quad, i: usize, source: &ID3D11ShaderResourceView, source_size: (u32, u32), cursor: (i32, i32), copy: &mut dyn FnMut() -> Option<(u32, u32)>) {
        // All of it at once: another thread's drawing in between would be
        // skipped by the predicate too.
        let _lock = gpu.exclusive();
        let at = host_now();
        let slot = &self.ring[i];
        quad.signature(gpu, &slot.signature.0, BLOCKS, source, source_size, SIGNATURE_ROWS);
        let forced = at - self.last_new.max(self.last_forced) > FORCE_AFTER;
        let (check, before, size) = match self.before {
            // (Compared by signature, `before`'s size is the source's.)
            Some((b, before_size, before_at)) if before_size == source_size && !forced => unsafe {
                gpu.context.Begin(&slot.predicate);
                quad.changed(gpu, &self.blocks, BLOCKS, &slot.signature.1, &self.ring[b].signature.1);
                gpu.context.End(&slot.predicate);
                // The copy is skipped when the predicate is false: nothing passed.
                gpu.context.SetPredication(Some(&slot.predicate), false);
                let size = copy();
                gpu.context.SetPredication(None, false);
                (Check::Predicate, before_at, size)
            },
            Some((_, _, before_at)) => {
                self.last_forced = at;
                (Check::Forced, before_at, copy())
            }
            None => (Check::First, at, copy()),
        };
        unsafe { gpu.context.Flush() };
        let Some(size) = size else { return };
        self.push(Pending { slot: i, at, before, size, cursor, check }, source_size);
    }

    /// A look made: to resolve. `compare_size`: what the next look's must be
    /// to be compared with it.
    fn push(&mut self, p: Pending, compare_size: (u32, u32)) {
        let (i, at) = (p.slot, p.at);
        self.pending.push_back(p);
        self.counted.1 += 1;
        if self.counted.0.elapsed() >= std::time::Duration::from_secs(5) {
            if std::env::var_os("HASTECLIP_HOOK_LOG").is_some() {
                let secs = self.counted.0.elapsed().as_secs_f64();
                eprintln!("game hook: {:.0} looks/s, {:.0} new pictures/s, {} comparisons unread", self.counted.1 as f64 / secs, self.counted.2 as f64 / secs, self.counted.3);
            }
            self.counted = (std::time::Instant::now(), 0, 0, 0);
        }
        self.before = Some((i, compare_size, at));
        self.next = (i + 1) % RING;
    }

    /// The comparisons that have arrived: a look that brought a new picture
    /// (or the first) puts it up in `latest`, timed, with the cursor drawn on
    /// it by `draw_cursor` (given where to draw). The same picture with the
    /// cursor moved goes up too, but only counts as a new picture for frame
    /// blending once the game has drawn nothing new for a while (a menu).
    pub(crate) fn resolve(&mut self, gpu: &Gpu, latest: &Latest, latest_target: &ID3D11RenderTargetView, draw_cursor: &mut dyn FnMut(&ID3D11RenderTargetView, (u32, u32))) {
        while let Some(p) = self.pending.front() {
            // Whether it brought a new picture, and whether its slot holds it.
            let (mut differs, copied) = match p.check {
                Check::First => (true, true),
                Check::Forced => (false, true),
                Check::Full => (false, true),
                Check::Predicate => {
                    // Untouched while the answer isn't there yet.
                    let mut answer = u32::MAX;
                    let _ = unsafe { gpu.context.GetData(&self.ring[p.slot].predicate, Some(&mut answer as *mut u32 as *mut _), 4, D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32) };
                    if answer == u32::MAX {
                        return;
                    }
                    (answer != 0, answer != 0)
                }
            };
            if matches!(p.check, Check::Full) {
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
            if copied {
                self.copied = Some(p.slot);
            }
            let forced = matches!(p.check, Check::Forced);
            let moved = self.last_cursor != Some(p.cursor);
            if !(differs || moved || forced) {
                continue;
            }
            // The cursor moved over the same picture: the last one copied.
            let Some(slot) = (if copied { Some(p.slot) } else { self.copied }) else { continue };
            self.last_cursor = Some(p.cursor);
            // It came up after the look before and by this one.
            let at = (p.before + p.at) / 2.0;
            let counts = differs || at - self.last_new > self.quiet;
            if counts {
                self.last_new = at;
            }
            self.counted.2 += differs as u32;
            unsafe { gpu.context.CopyResource(&latest.texture, &self.ring[slot].picture.texture) };
            draw_cursor(latest_target, p.size);
            latest.mark(p.size, at, counts);
            unsafe { gpu.context.Flush() };
        }
    }
}
