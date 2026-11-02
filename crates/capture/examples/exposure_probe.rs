//! Which exposure controls a camera's driver has on Windows, and in what
//! steps: `cargo run -p capture --example exposure_probe -- [name part]`.
//! `SET=<100 ns units>` also sets the extended one manually, reads it back
//! (and DirectShow's), and goes back to automatic.
#[cfg(target_os = "windows")]
fn main() {
    use windows::Win32::Media::KernelStreaming::*;
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::core::Interface;

    let part = std::env::args().nth(1).unwrap_or_else(|| "c922".into());
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap();
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 1).unwrap();
        let attrs = attrs.unwrap();
        attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID).unwrap();
        let mut list = std::ptr::null_mut();
        let mut count = 0;
        MFEnumDeviceSources(&attrs, &mut list, &mut count).unwrap();
        let devices: Vec<IMFActivate> = std::slice::from_raw_parts(list, count as usize).iter().filter_map(|a| a.clone()).collect();
        let name = |a: &IMFActivate| {
            let mut p = windows::core::PWSTR::null();
            let mut len = 0;
            a.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut p, &mut len).ok().and_then(|_| p.to_string().ok()).unwrap_or_default()
        };
        let Some(dev) = devices.iter().find(|a| name(a).to_lowercase().contains(&part.to_lowercase())) else {
            println!("no camera with {part:?} in its name");
            return;
        };
        println!("{}", name(dev));
        let source: IMFMediaSource = dev.ActivateObject().unwrap();
        let ks: IKsControl = source.cast().unwrap();

        // DirectShow's: powers of two of a second.
        let legacy = |kind: u32, value: i32, flags: u32| {
            let mut p = KSPROPERTY_CAMERACONTROL_S { Value: value, Flags: flags, ..Default::default() };
            p.Property.Anonymous.Anonymous = KSIDENTIFIER_0_0 { Set: PROPSETID_VIDCAP_CAMERACONTROL, Id: KSPROPERTY_CAMERACONTROL_EXPOSURE.0 as u32, Flags: kind };
            let mut out = p;
            let size = std::mem::size_of::<KSPROPERTY_CAMERACONTROL_S>() as u32;
            let mut got = 0;
            ks.KsProperty(&p.Property, size, (&raw mut out).cast(), size, &mut got).map(|_| (out.Value, out.Flags))
        };
        println!("DirectShow exposure (log2 s): {:?}", legacy(KSPROPERTY_TYPE_GET, 0, 0));
        // Its range and step, and whether the relative one (UVC's step by
        // one of the camera's own units) is there: their basic support.
        for (what, id) in [("exposure", KSPROPERTY_CAMERACONTROL_EXPOSURE.0), ("exposure, relative", KSPROPERTY_CAMERACONTROL_EXPOSURE_RELATIVE.0)] {
            let mut p = KSIDENTIFIER::default();
            p.Anonymous.Anonymous = KSIDENTIFIER_0_0 { Set: PROPSETID_VIDCAP_CAMERACONTROL, Id: id as u32, Flags: KSPROPERTY_TYPE_BASICSUPPORT };
            let mut buf = [0i32; 32];
            let mut got = 0;
            let r = ks.KsProperty(&p, std::mem::size_of::<KSPROPERTY_CAMERACONTROL_S>() as u32, buf.as_mut_ptr().cast(), 128, &mut got);
            // After the description (10 words) and members header (4): step, reserved, min, max.
            println!("{what}: {} ({got} bytes) step {} min {} max {}", r.map_or_else(|e| format!("{e}"), |_| "ok".into()), buf[14], buf[16], buf[17]);
        }

        // The extended one: 100 ns units.
        #[repr(C)]
        #[derive(Clone, Copy, Default)]
        struct Ext {
            head: KSCAMERA_EXTENDEDPROP_HEADER,
            setting: KSCAMERA_EXTENDEDPROP_VIDEOPROCSETTING,
        }
        let ext = |kind: u32, set: Option<(u64, i64)>| {
            let mut p = KSIDENTIFIER::default();
            p.Anonymous.Anonymous = KSIDENTIFIER_0_0 { Set: KSPROPERTYSETID_ExtendedCameraControl, Id: KSPROPERTY_CAMERACONTROL_EXTENDED_EXPOSUREMODE.0 as u32, Flags: kind };
            let mut data = Ext::default();
            data.head.Version = 1;
            data.head.PinId = KSCAMERA_EXTENDEDPROP_FILTERSCOPE;
            data.head.Size = std::mem::size_of::<Ext>() as u32;
            if let Some((flags, value)) = set {
                data.head.Flags = flags;
                data.setting.VideoProc.Value.ll = value;
            }
            let mut got = 0;
            let r = ks.KsProperty(&p, std::mem::size_of::<KSIDENTIFIER>() as u32, (&raw mut data).cast(), std::mem::size_of::<Ext>() as u32, &mut got);
            r.map(|_| (got, data))
        };
        let show = |what: &str, r: &windows::core::Result<(u32, Ext)>| match r {
            Ok((got, d)) => println!(
                "{what}: {got} bytes, flags {:#x} caps {:#x} result {:#x}, mode {:#x} min {} max {} step {} (100 ns), value {}",
                d.head.Flags,
                d.head.Capability,
                d.head.Result,
                d.setting.Mode,
                d.setting.Min,
                d.setting.Max,
                d.setting.Step,
                d.setting.VideoProc.Value.ll
            ),
            Err(e) => println!("{what}: {e}"),
        };
        show("extended exposure", &ext(KSPROPERTY_TYPE_GET, None));

        // Every extended control, asked how big it is (no buffer), on the
        // filter and its first pins: which the driver has at all.
        for id in 0..48u32 {
            let mut p = KSIDENTIFIER::default();
            p.Anonymous.Anonymous = KSIDENTIFIER_0_0 { Set: KSPROPERTYSETID_ExtendedCameraControl, Id: id, Flags: KSPROPERTY_TYPE_GET };
            let mut got = 0;
            let r = ks.KsProperty(&p, std::mem::size_of::<KSIDENTIFIER>() as u32, std::ptr::null_mut(), 0, &mut got);
            if got > 0 || r.as_ref().is_err_and(|e| e.code().0 as u32 != 0x80070490 && e.code().0 as u32 != 0x80070032) {
                println!("extended control {id}: {got} bytes ({})", r.map_or_else(|e| format!("{e}"), |_| "ok".into()));
            }
        }
        for pin in [0u32, 1, 2] {
            let mut p = KSIDENTIFIER::default();
            p.Anonymous.Anonymous = KSIDENTIFIER_0_0 { Set: KSPROPERTYSETID_ExtendedCameraControl, Id: KSPROPERTY_CAMERACONTROL_EXTENDED_EXPOSUREMODE.0 as u32, Flags: KSPROPERTY_TYPE_GET };
            let mut data = Ext::default();
            data.head.Version = 1;
            data.head.PinId = pin;
            data.head.Size = std::mem::size_of::<Ext>() as u32;
            let mut got = 0;
            let r = ks.KsProperty(&p, std::mem::size_of::<KSIDENTIFIER>() as u32, (&raw mut data).cast(), std::mem::size_of::<Ext>() as u32, &mut got);
            show(&format!("extended exposure, pin {pin}"), &r.map(|_| (got, data)));
        }

        if let Some(v) = std::env::var("SET").ok().and_then(|s| s.parse::<i64>().ok()) {
            show("set manual", &ext(KSPROPERTY_TYPE_SET, Some((KSCAMERA_EXTENDEDPROP_VIDEOPROCFLAG_MANUAL, v))));
            std::thread::sleep(std::time::Duration::from_millis(300));
            show("read back", &ext(KSPROPERTY_TYPE_GET, None));
            println!("DirectShow now: {:?}", legacy(KSPROPERTY_TYPE_GET, 0, 0));
            show("back to automatic", &ext(KSPROPERTY_TYPE_SET, Some((KSCAMERA_EXTENDEDPROP_VIDEOPROCFLAG_AUTO, 0))));
            println!("DirectShow after: {:?}", legacy(KSPROPERTY_TYPE_GET, 0, 0));
        }
        let _ = source.Shutdown();
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {}
