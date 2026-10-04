//! Does the Desktop Duplication API see a fullscreen game? Duplicates display
//! `OUTPUT` (default 0) for `SECS` (default 8) and counts the frames whose
//! picture actually changed (a 256×256 patch at the centre, compared).
//! `WAIT_FOR=<process name>` starts once that process's window is in front.
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::*;
    use windows::core::Interface;

    if let Ok(name) = std::env::var("WAIT_FOR") {
        use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
        println!("waiting for {name} to be in front");
        loop {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid)) };
            let front = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains(&name.to_lowercase()))
                .unwrap_or(false);
            if front {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let output_index: u32 = std::env::var("OUTPUT").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let secs: f64 = std::env::var("SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(8.0);
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        // The adapter with the display.
        let mut found = None;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            if let Ok(out) = adapter.EnumOutputs(output_index) {
                found = Some((adapter, out));
                break;
            }
            a += 1;
        }
        let (adapter, output) = found.ok_or_else(|| anyhow::anyhow!("no display {output_index}"))?;
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(&adapter, D3D_DRIVER_TYPE_UNKNOWN, Default::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None, D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context))?;
        let (device, context) = (device.unwrap(), context.unwrap());
        let output1: IDXGIOutput1 = output.cast()?;
        let mut dupl = output1.DuplicateOutput(&device)?;
        let desc = dupl.GetDesc();
        println!("duplicating {}x{}", desc.ModeDesc.Width, desc.ModeDesc.Height);
        let patch = 256u32;
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: patch,
            Height: patch,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        let staging = staging.unwrap();
        let (cx, cy) = (desc.ModeDesc.Width / 2 - patch / 2, desc.ModeDesc.Height / 2 - patch / 2);
        let start = Instant::now();
        let (mut frames, mut changed, mut lost, mut timeouts) = (0u32, 0u32, 0u32, 0u32);
        let mut last_hash = 0u64;
        while start.elapsed().as_secs_f64() < secs {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res = None;
            match dupl.AcquireNextFrame(100, &mut info, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    timeouts += 1;
                    continue;
                }
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST || e.code() == DXGI_ERROR_INVALID_CALL => {
                    lost += 1;
                    if lost <= 3 {
                        println!("{:.1} s: duplication lost ({})", start.elapsed().as_secs_f64(), e.message());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    match output1.DuplicateOutput(&device) {
                        Ok(d) => dupl = d,
                        Err(e) if lost <= 3 => println!("  starting again failed: {} ({:#x})", e.message(), e.code().0),
                        Err(_) => {}
                    }
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
            if info.LastPresentTime != 0 {
                frames += 1;
                let tex: ID3D11Texture2D = res.unwrap().cast()?;
                let region = D3D11_BOX { left: cx, top: cy, front: 0, right: cx + patch, bottom: cy + patch, back: 1 };
                context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &tex, 0, Some(&region));
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
                let mut h = 0xcbf29ce484222325u64;
                for y in 0..patch as usize {
                    let row = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), patch as usize * 4);
                    for &b in row.iter().step_by(7) {
                        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
                    }
                }
                context.Unmap(&staging, 0);
                if h != last_hash {
                    changed += 1;
                    last_hash = h;
                }
            }
            let _ = dupl.ReleaseFrame();
        }
        println!("{frames} new frames in {secs} s, {changed} with a changed picture; access lost {lost}×, idle waits {timeouts}");
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
