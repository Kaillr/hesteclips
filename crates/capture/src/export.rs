//! Saving an edited clip's picture with the graphics card, for `media`'s
//! export where the picture is made again: on Linux (`linux::export`);
//! elsewhere, for now, ffmpeg makes it.

use std::path::Path;

use anyhow::Result;

use crate::BlendShape;

/// What to make.
pub struct Job<'a> {
    pub source: &'a Path,
    /// The source's frame rate, which its frames are numbered on.
    pub fps: f64,
    /// The parts kept, in the source's seconds, in order.
    pub parts: &'a [(f64, f64)],
    /// The part of the picture kept: left, top, width, height in its pixels.
    pub crop: Option<[u32; 4]>,
    /// The frames' size (even) and rate.
    pub size: (u32, u32),
    pub out_fps: u32,
    /// Frame blending: the rate looked at (a multiple of `out_fps`), and how
    /// the looks in a frame are weighed.
    pub blend: Option<(u32, BlendShape)>,
    pub kbps: u32,
    /// To fit a size: x264, held to `kbps` over short spans.
    pub fit: bool,
}

/// Make `job`'s picture (no sound) at `dest`, an MP4. `Ok(false)` where it
/// can't be made here: ffmpeg makes it then.
pub fn render_video(job: &Job, dest: &Path, progress: &dyn Fn(f32)) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::export::render_video(job, dest, progress)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (job, dest, progress);
        Ok(false)
    }
}
