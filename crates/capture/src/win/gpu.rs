//! How busy each process keeps one of the graphics card's engines, from
//! Windows' performance counters (what Task Manager's GPU column shows; no
//! admin rights needed).
//!
//! - Drawing (its 3D engine) tells a game that's drawing from one that
//!   isn't: Windows' capture gets a frame each time a window presents one,
//!   even the same picture again, so a game that's drawing while Windows'
//!   capture gets nothing is one Windows can't see (exclusive fullscreen).
//! - Decoding video tells whether another app is playing some (a browser):
//!   the card's video decoder takes turns between everything decoding on it,
//!   and scrubbing leaves it room.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData,
    PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows::core::{PCWSTR, w};

/// An engine's use per process, sampled at most once a second.
pub struct GpuUse {
    query: PDH_HQUERY,
    counters: Vec<PDH_HCOUNTER>,
    sampled: Instant,
    /// Percent of the engine, per process id, over the last sample.
    by_pid: HashMap<u32, f64>,
}
unsafe impl Send for GpuUse {}

/// How often the counters are read: they're averages since the last read.
const EVERY: Duration = Duration::from_secs(1);

impl GpuUse {
    /// The 3D engine's (drawing).
    pub fn new() -> Option<Self> {
        Self::of(&[w!(r"\GPU Engine(*engtype_3D)\Utilization Percentage")])
    }

    /// The video decoder's (playing video), all of them on a card with more
    /// than one (numbered after the first: `VideoDecode_1`). Named as Task
    /// Manager names an engine, spaces left out: NVIDIA's and Intel's
    /// decoders are Video Decode; AMD's decode and encode on one, Video
    /// Codec.
    pub fn video_decode() -> Option<Self> {
        Self::of(&[w!(r"\GPU Engine(*engtype_VideoDecode*)\Utilization Percentage"), w!(r"\GPU Engine(*engtype_VideoCodec*)\Utilization Percentage")])
    }

    /// `paths`: every process's every engine of a kind; the instance names
    /// start with `pid_<id>_`. None if no path could be read.
    fn of(paths: &[PCWSTR]) -> Option<Self> {
        let mut query = PDH_HQUERY::default();
        unsafe {
            if PdhOpenQueryW(None, 0, &mut query) != 0 {
                return None;
            }
            let counters: Vec<PDH_HCOUNTER> = paths
                .iter()
                .filter_map(|&path| {
                    let mut counter = PDH_HCOUNTER::default();
                    (PdhAddEnglishCounterW(query, path, 0, &mut counter) == 0).then_some(counter)
                })
                .collect();
            if counters.is_empty() {
                let _ = PdhCloseQuery(query);
                return None;
            }
            // The first read only sets where the averages start.
            PdhCollectQueryData(query);
            Some(Self { query, counters, sampled: Instant::now(), by_pid: HashMap::new() })
        }
    }

    /// Percent of the engine process `pid` used lately (0 until there's a
    /// sample).
    pub fn percent(&mut self, pid: u32) -> f64 {
        if self.sampled.elapsed() >= EVERY {
            self.sample();
        }
        self.by_pid.get(&pid).copied().unwrap_or(0.0)
    }

    /// Percent of the engine every process but `pid` used since the last
    /// read (or since it was made), read now: the caller keeps the time.
    pub fn others_now(&mut self, pid: u32) -> f64 {
        self.sample();
        self.by_pid.iter().filter(|&(&p, _)| p != pid).map(|(_, v)| v).sum()
    }

    fn sample(&mut self) {
        self.sampled = Instant::now();
        self.by_pid.clear();
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return;
            }
            for &counter in &self.counters {
                let (mut size, mut count) = (0u32, 0u32);
                // Nothing more when no engine has that name (another make's).
                if PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, None) != PDH_MORE_DATA {
                    continue;
                }
                // Bytes, for the items and the names they point to.
                let mut buf = vec![0u64; (size as usize).div_ceil(8)];
                let items = buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
                if PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) != 0 {
                    continue;
                }
                for item in std::slice::from_raw_parts(items, count as usize) {
                    let Ok(name) = item.szName.to_string() else { continue };
                    let Some(pid) = name.strip_prefix("pid_").and_then(|r| r.split('_').next()).and_then(|p| p.parse::<u32>().ok()) else { continue };
                    if item.FmtValue.CStatus != 0 {
                        continue;
                    }
                    *self.by_pid.entry(pid).or_default() += item.FmtValue.Anonymous.doubleValue;
                }
            }
        }
    }
}

impl Drop for GpuUse {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}
