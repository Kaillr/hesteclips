//! How busy each process keeps the graphics card drawing (its 3D engine), from
//! Windows' performance counters (what Task Manager's GPU column shows; no
//! admin rights needed). Tells a game that's drawing from one that isn't:
//! Windows' capture gets a frame each time a window presents one, even the
//! same picture again, so a game that's drawing while Windows' capture gets
//! nothing is one Windows can't see (exclusive fullscreen).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData,
    PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows::core::w;

/// The 3D engine's use per process, sampled at most once a second.
pub struct GpuUse {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    sampled: Instant,
    /// Percent of the 3D engine, per process id, over the last sample.
    by_pid: HashMap<u32, f64>,
}
unsafe impl Send for GpuUse {}

/// How often the counters are read: they're averages since the last read.
const EVERY: Duration = Duration::from_secs(1);

impl GpuUse {
    pub fn new() -> Option<Self> {
        let mut query = PDH_HQUERY::default();
        let mut counter = PDH_HCOUNTER::default();
        unsafe {
            if PdhOpenQueryW(None, 0, &mut query) != 0 {
                return None;
            }
            // Every process's every 3D engine; the instance names start
            // with `pid_<id>_`.
            if PdhAddEnglishCounterW(query, w!(r"\GPU Engine(*engtype_3D)\Utilization Percentage"), 0, &mut counter) != 0 {
                let _ = PdhCloseQuery(query);
                return None;
            }
            // The first read only sets where the averages start.
            PdhCollectQueryData(query);
        }
        Some(Self { query, counter, sampled: Instant::now(), by_pid: HashMap::new() })
    }

    /// Percent of the graphics card's 3D engine process `pid` used lately (0
    /// until there's a sample).
    pub fn percent(&mut self, pid: u32) -> f64 {
        if self.sampled.elapsed() >= EVERY {
            self.sample();
        }
        self.by_pid.get(&pid).copied().unwrap_or(0.0)
    }

    fn sample(&mut self) {
        self.sampled = Instant::now();
        self.by_pid.clear();
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return;
            }
            let (mut size, mut count) = (0u32, 0u32);
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None) != PDH_MORE_DATA {
                return;
            }
            // Bytes, for the items and the names they point to.
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let items = buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) != 0 {
                return;
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

impl Drop for GpuUse {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}
