//! The editor's export settings: how the saved clip is made, when it
//! shouldn't be just like the recording. Smaller (resolution, frame rate,
//! quality), small enough to send (fit under a size, like Discord's 10 MB),
//! or with only the mix for audio. Saved with the edit, so they come back
//! when the clip is opened again.

use media::{ClipInfo, Edit, Output};

/// Common heights, offered when smaller than the clip.
const HEIGHTS: [u32; 5] = [2160, 1440, 1080, 720, 480];
const RATES: [u32; 3] = [60, 30, 24];
/// Quality presets, as a share of the clip's own bitrate (`None`: the clip's own).
const QUALITY: [(&str, f32); 3] = [("High", 0.6), ("Medium", 0.35), ("Low", 0.15)];
/// Sizes to fit under, in MB: Discord's free limit first.
const SIZES: [(u32, &str); 4] = [(10, "10 MB (Discord)"), (25, "25 MB"), (50, "50 MB"), (100, "100 MB")];

/// A one-line summary for the button that opens the settings: "Original",
/// or what changes ("1080p · 30 fps · under 10 MB").
pub fn summary(info: &ClipInfo, edit: &Edit) -> String {
    let o = edit.output;
    let mut parts = Vec::new();
    if o.height.is_some_and(|h| h < info.height) {
        parts.push(format!("{}p", o.height.unwrap()));
    }
    if o.fps.is_some_and(|f| (f as f64) < info.fps - 0.5) {
        parts.push(match o.blend_fps {
            Some(b) => format!("{} fps blended from {b}", o.fps.unwrap()),
            None => format!("{} fps", o.fps.unwrap()),
        });
    }
    if let Some(mb) = o.max_mb {
        parts.push(format!("under {mb} MB"));
    } else if let Some(k) = o.video_kbps.filter(|&k| info.video_kbps.is_none_or(|s| k < s)) {
        parts.push(mbps(k));
    }
    if o.mix_only && edit.tracks.len() > 1 {
        parts.push("mix only".into());
    }
    if parts.is_empty() { "Original quality".into() } else { parts.join(" · ") }
}

/// The settings, in a popup. True if anything changed.
pub fn ui(ui: &mut egui::Ui, info: &ClipInfo, edit: &mut Edit) -> bool {
    use crate::theme::{self, W};
    use crate::ui_kit::{self, Item};
    let pal = theme::pal(ui);
    let before = edit.output;
    // Trim and tracks, for sizes worked out while the settings change.
    let mut dims = Edit { start: edit.start, end: edit.end, tracks: edit.tracks.clone(), output: edit.output };
    let tracks = edit.tracks.len();
    let o = &mut edit.output;
    ui.set_width(330.0);
    ui.label(theme::text("Export", 14.5, W::Semibold).color(pal.text));
    ui.add_space(2.0);
    ui.label(theme::text("How the saved clip is made. Smaller files send and upload faster.", 12.5, W::Regular).color(pal.text3));
    ui.add_space(10.0);
    // A row: its name on the left, its control on the right.
    let row = |ui: &mut egui::Ui, label: &str, add: &mut dyn FnMut(&mut egui::Ui)| {
        ui.horizontal(|ui| {
            ui.set_min_height(36.0);
            ui.label(theme::text(label, 13.5, W::Regular).color(pal.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| add(ui));
        });
    };
    // What a size target picked, shown where it decides for you.
    let auto = o.max_mb.map(|_| (o.size(info).1, o.frame_rate(info)));

    row(ui, "Fit under", &mut |ui| {
        let shown = match o.max_mb {
            None => "No limit".to_owned(),
            Some(mb) => SIZES.iter().find(|(s, _)| *s == mb).map_or(format!("{mb} MB"), |(_, l)| (*l).to_owned()),
        };
        let r = ui_kit::select(ui, &shown, 150.0);
        ui_kit::menu_popup(&r, false, 200.0, |ui| {
            if Item::new("No limit").check(o.max_mb.is_none()).show(ui).clicked() {
                o.max_mb = None;
            }
            for (mb, label) in SIZES {
                if Item::new(label).check(o.max_mb == Some(mb)).show(ui).clicked() {
                    o.max_mb = Some(mb);
                }
            }
        });
        if let Some(mb) = &mut o.max_mb {
            ui.add(egui::DragValue::new(mb).range(1..=4000).suffix(" MB").speed(1.0)).on_hover_text("Any size: drag, or click and type");
        }
    });

    row(ui, "Resolution", &mut |ui| {
        let label = |h: Option<u32>| match h {
            Some(h) => format!("{h}p"),
            None => format!("Original ({}p)", info.height),
        };
        let shown = match (o.height, auto) {
            (None, Some((h, _))) if h < info.height => format!("Auto ({h}p)"),
            (h, _) => label(h.filter(|&h| h < info.height)),
        };
        let r = ui_kit::select(ui, &shown, 150.0);
        ui_kit::menu_popup(&r, false, 200.0, |ui| {
            if Item::new(if auto.is_some() { "Auto".to_owned() } else { label(None) }).check(o.height.is_none()).show(ui).clicked() {
                o.height = None;
            }
            for h in HEIGHTS.into_iter().filter(|&h| h < info.height) {
                if Item::new(label(Some(h))).check(o.height == Some(h)).show(ui).clicked() {
                    o.height = Some(h);
                }
            }
        });
    });

    let src = info.fps.round() as u32;
    row(ui, "Frame rate", &mut |ui| {
        let shown = match (o.fps, auto) {
            (None, Some((_, f))) if f < info.fps - 0.5 => format!("Auto ({f:.0} fps)"),
            (Some(f), _) if f < src => format!("{f} fps"),
            _ => format!("Original ({src} fps)"),
        };
        let r = ui_kit::select(ui, &shown, 150.0);
        ui_kit::menu_popup(&r, false, 200.0, |ui| {
            if Item::new(if auto.is_some() { "Auto".to_owned() } else { format!("Original ({src} fps)") }).check(o.fps.is_none()).show(ui).clicked() {
                o.fps = None;
            }
            for f in RATES.into_iter().filter(|&f| f < src) {
                if Item::new(format!("{f} fps")).check(o.fps == Some(f)).show(ui).clicked() {
                    o.fps = Some(f);
                }
            }
        });
    });

    let out = auto.map_or(o.fps.filter(|&f| (f as f64) < info.fps - 0.5), |(_, f)| Some(f.round() as u32).filter(|&f| (f as f64) < info.fps - 0.5).or(o.fps)).unwrap_or(src);
    let rates = if out < src { media::blend_rates(out, info.fps) } else { Vec::new() };
    if !rates.is_empty() {
        row(ui, "Frame blending", &mut |ui| {
            let shown = match o.blend_fps.filter(|b| rates.contains(b)) {
                Some(b) => format!("From {b} fps"),
                None => "Off".to_owned(),
            };
            let r = ui_kit::select(ui, &shown, 150.0);
            ui_kit::menu_popup(&r, false, 220.0, |ui| {
                if Item::new("Off").check(o.blend_fps.is_none()).tip("Drop the in-between frames").show(ui).clicked() {
                    o.blend_fps = None;
                }
                for b in &rates {
                    if Item::new(format!("From {b} fps")).check(o.blend_fps == Some(*b)).tip("Each frame is the average of the recording's frames in its slot, for smoother motion").show(ui).clicked() {
                        o.blend_fps = Some(*b);
                    }
                }
            });
        });
    }

    row(ui, "Quality", &mut |ui| {
        if o.max_mb.is_some() {
            ui.label(theme::text("Set by the size", 13.0, W::Regular).color(pal.text3)).on_hover_text("A size limit sets the quality");
            return;
        }
        let src = info.video_kbps;
        let preset = |share: f32| src.map(|s| (s as f32 * share) as u32);
        let shown = match o.video_kbps {
            None => match src {
                Some(s) if o.changes_video(info) => {
                    dims.output = *o;
                    format!("Auto ({})", mbps(o.video_kbps_for(info, &dims).unwrap_or(s)))
                }
                Some(s) => format!("Original ({})", mbps(s)),
                None => "Original".to_owned(),
            },
            Some(k) => QUALITY.iter().find(|(_, sh)| preset(*sh) == Some(k)).map_or(mbps(k), |(n, _)| format!("{n} ({})", mbps(k))),
        };
        let r = ui_kit::select(ui, &shown, 150.0);
        ui_kit::menu_popup(&r, false, 220.0, |ui| {
            if Item::new("Original").check(o.video_kbps.is_none()).tip("The recording's own quality, scaled down with a smaller picture").show(ui).clicked() {
                o.video_kbps = None;
            }
            for (name, share) in QUALITY {
                if let Some(k) = preset(share) {
                    if Item::new(format!("{name} ({})", mbps(k))).check(o.video_kbps == Some(k)).show(ui).clicked() {
                        o.video_kbps = Some(k);
                    }
                }
            }
            ui_kit::menu_sep(ui);
            if Item::new("Type your own…").icon(egui_phosphor::regular::TEXT_T).show(ui).clicked() {
                o.video_kbps = Some(o.video_kbps.unwrap_or(8_000));
            }
        });
        if let Some(k) = &mut o.video_kbps {
            let mut m = *k as f32 / 1000.0;
            if ui.add(egui::DragValue::new(&mut m).range(0.5..=200.0).speed(0.1).max_decimals(1).suffix(" Mbps")).changed() {
                *k = (m * 1000.0).round() as u32;
            }
        }
    });

    if tracks > 1 {
        row(ui, "Sound", &mut |ui| {
            let opts = [
                ui_kit::Seg { icon: None, label: "All tracks", tip: Some("The mix, then each source on its own track, to rebalance later") },
                ui_kit::Seg { icon: None, label: "Mix only", tip: Some("Just what you hear: smaller, and some apps only play one track anyway") },
            ];
            if let Some(i) = ui_kit::segmented_opts(ui, "export_audio", &opts, usize::from(o.mix_only), None, true) {
                o.mix_only = i == 1;
            }
        });
    }

    // What it comes to.
    ui.add_space(8.0);
    let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(r.x_range(), r.center().y, egui::Stroke::new(1.0, pal.line));
    ui.add_space(10.0);
    let o = edit.output;
    ui.horizontal(|ui| {
        match o.estimate_bytes(info, edit) {
            Some(b) => {
                ui.label(theme::text(format!("About {}", human_bytes(b)), 13.0, W::Semibold).color(pal.text));
                let (w, h) = o.size(info);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(theme::text(format!("{w}×{h} at {:.0} fps", o.frame_rate(info)), 13.0, W::Regular).color(pal.text3));
                });
            }
            None => {
                ui.label(theme::text("Size unknown", 13.0, W::Regular).color(pal.text3));
            }
        }
    });
    if o.changes_video(info) {
        ui.add_space(4.0);
        ui_kit::hint(ui, "Every frame is encoded again, so saving takes longer.");
    }
    if o != Output::default() {
        ui.add_space(6.0);
        if ui_kit::Btn::ghost("Reset to original").small().show(ui).clicked() {
            edit.output = Output::default();
        }
    }
    edit.output != before
}

fn mbps(kbps: u32) -> String {
    if kbps >= 10_000 { format!("{:.0} Mbps", kbps as f32 / 1000.0) } else { format!("{:.1} Mbps", kbps as f32 / 1000.0) }
}

pub(crate) fn human_bytes(b: u64) -> String {
    let mb = b as f64 / 1024.0 / 1024.0;
    if mb >= 1024.0 { format!("{:.1} GB", mb / 1024.0) } else if mb >= 10.0 { format!("{mb:.0} MB") } else { format!("{mb:.1} MB") }
}
