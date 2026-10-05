//! File writing on Windows and Linux: the `FileWriter` behind `crate::writer`,
//! on our own MP4 muxer (`crate::mp4mux`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::mixer::RATE;
use crate::mp4mux::{AvcConfig, Data, Muxer, Spec};
use crate::writer::Media;

/// An encoded H.264 frame (AVCC), shared between the replay ring and the clips
/// being saved from it.
#[derive(Clone)]
pub(crate) struct VideoFrame(pub Arc<EncodedFrame>);

pub(crate) struct EncodedFrame {
    pub data: Data,
    /// Parameter sets; every keyframe carries them.
    pub config: Option<AvcConfig>,
}

/// What the writer needs to create its outputs.
#[derive(Clone)]
pub(crate) struct Layout {
    pub spec: Spec,
}

impl Layout {
    pub(crate) fn audio_tracks(&self) -> usize {
        self.spec.audio_titles.len()
    }
}

pub(crate) struct FileWriter {
    path: PathBuf,
    spec: Spec,
    /// Capture time of the file's first frame; written as t=0.
    start: f64,
    /// Recording (crash-safe fragments) rather than a clip.
    live: bool,
    /// Opened at the first keyframe, which carries the parameter sets.
    mux: Option<Muxer>,
}

impl FileWriter {
    pub(crate) fn create(path: &Path, layout: &Layout, start: f64, _fps: u32, live: bool) -> Result<Self> {
        let _ = std::fs::remove_file(path);
        Ok(Self { path: path.to_path_buf(), spec: layout.spec.clone(), start, live, mux: None })
    }

    pub(crate) fn append(&mut self, media: &Media) -> Result<()> {
        match media {
            Media::Video { frame, pts, key } => {
                if self.mux.is_none() {
                    if !key {
                        return Ok(());
                    }
                    let config = frame.0.config.clone().context("the encoder gave no H.264 parameter sets")?;
                    self.mux = Some(Muxer::create(&self.path, &self.spec, config, self.live)?);
                }
                self.mux.as_mut().unwrap().video(frame.0.data.clone(), pts - self.start, *key)
            }
            Media::Audio { track, packet } => {
                let Some(mux) = self.mux.as_mut() else { return Ok(()) };
                let frame = packet.frame - (self.start * RATE as f64).round() as i64;
                if frame < 0 {
                    return Ok(());
                }
                mux.audio(*track, Arc::from(packet.data.as_slice()), frame)
            }
        }
    }

    pub(crate) fn finish(self) -> Result<()> {
        match self.mux {
            Some(mux) => mux.finish().with_context(|| format!("couldn't finish {}", self.path.display())),
            None => bail!("nothing was recorded"),
        }
    }
}
