//! The editor's export settings: how the saved clip is made, when it
//! shouldn't be just like the recording. Smaller (resolution, frame rate,
//! quality), small enough to send (fit under a size, like Discord's 10 MB),
//! or with only the mix for audio. Saved with the edit, so they come back
//! when the clip is opened again.

use egui::{Color32, RichText};
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
        parts.push(format!("{} fps", o.fps.unwrap()));
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
    let before = edit.output;
    // Trim and tracks, for sizes worked out while the settings change.
    let mut dims = Edit { start: edit.start, end: edit.end, tracks: edit.tracks.clone(), output: edit.output };
    let tracks = edit.tracks.len();
    let o = &mut edit.output;
    ui.set_width(330.0);
    ui.label(RichText::new("Export settings").strong().size(15.0));
    ui.weak("How the saved clip is made. Smaller files send and upload faster.");
    ui.add_space(8.0);

    let grid = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
        egui::Grid::new("export_grid").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| add(ui));
    };
    // What a size target picked, shown greyed where it decides for you.
    let auto = o.max_mb.map(|_| (o.size(info).1, o.frame_rate(info)));

    grid(ui, &mut |ui| {
        ui.label("Resolution");
        let label = |h: Option<u32>| match h {
            Some(h) => format!("{h}p"),
            None => format!("Original ({}p)", info.height),
        };
        let shown = match (o.height, auto) {
            (None, Some((h, _))) if h < info.height => format!("Auto ({h}p)"),
            (h, _) => label(h.filter(|&h| h < info.height)),
        };
        egui::ComboBox::from_id_salt("export_height").width(170.0).selected_text(shown).show_ui(ui, |ui| {
            ui.selectable_value(&mut o.height, None, if auto.is_some() { "Auto".to_owned() } else { label(None) });
            for h in HEIGHTS.into_iter().filter(|&h| h < info.height) {
                ui.selectable_value(&mut o.height, Some(h), label(Some(h)));
            }
        });
        ui.end_row();

        ui.label("Frame rate");
        let src = info.fps.round() as u32;
        let shown = match (o.fps, auto) {
            (None, Some((_, f))) if f < info.fps - 0.5 => format!("Auto ({f:.0} fps)"),
            (Some(f), _) if f < src => format!("{f} fps"),
            _ => format!("Original ({src} fps)"),
        };
        egui::ComboBox::from_id_salt("export_fps").width(170.0).selected_text(shown).show_ui(ui, |ui| {
            ui.selectable_value(&mut o.fps, None, if auto.is_some() { "Auto".to_owned() } else { format!("Original ({src} fps)") });
            for f in RATES.into_iter().filter(|&f| f < src) {
                ui.selectable_value(&mut o.fps, Some(f), format!("{f} fps"));
            }
        });
        ui.end_row();

        ui.label("Fit under");
        let shown = match o.max_mb {
            None => "No limit".to_owned(),
            Some(mb) => SIZES.iter().find(|(s, _)| *s == mb).map_or(format!("{mb} MB"), |(_, l)| (*l).to_owned()),
        };
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("export_size").width(130.0).selected_text(shown).show_ui(ui, |ui| {
                ui.selectable_value(&mut o.max_mb, None, "No limit");
                for (mb, label) in SIZES {
                    ui.selectable_value(&mut o.max_mb, Some(mb), label);
                }
            });
            if let Some(mb) = &mut o.max_mb {
                ui.add(egui::DragValue::new(mb).range(1..=4000).suffix(" MB").speed(1.0));
            }
        });
        ui.end_row();

        ui.label("Quality");
        ui.add_enabled_ui(o.max_mb.is_none(), |ui| {
            let src = info.video_kbps;
            let preset = |share: f32| src.map(|s| (s as f32 * share) as u32);
            let shown = match o.video_kbps {
                _ if o.max_mb.is_some() => "Set by the size".to_owned(),
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
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("export_quality").width(130.0).selected_text(shown).show_ui(ui, |ui| {
                    ui.selectable_value(&mut o.video_kbps, None, "Original / auto")
                        .on_hover_text("The recording's own quality, scaled down with a smaller picture");
                    for (name, share) in QUALITY {
                        if let Some(k) = preset(share) {
                            ui.selectable_value(&mut o.video_kbps, Some(k), format!("{name} ({})", mbps(k)));
                        }
                    }
                    if src.is_none() || ui.selectable_label(false, "Custom…").clicked() {
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
        });
        ui.end_row();

        if tracks > 1 {
            ui.label("Audio");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut o.mix_only, false, "All tracks").on_hover_text("The mix, then each source on its own track, to rebalance later");
                ui.selectable_value(&mut o.mix_only, true, "Mix only").on_hover_text("Just what you hear: smaller, and some apps only play one track anyway");
            });
            ui.end_row();
        }
    });

    ui.add_space(10.0);
    let o = edit.output;
    match o.estimate_bytes(info, edit) {
        Some(b) => {
            let (w, h) = o.size(info);
            ui.label(RichText::new(format!("About {}  ·  {w}×{h}, {:.0} fps", human_bytes(b), o.frame_rate(info))).strong());
        }
        None => {
            ui.weak("Size unknown");
        }
    }
    if o.changes_video(info) {
        ui.weak("Every frame is encoded again, so saving takes longer (about the clip's length at full size).");
    }
    if o != Output::default() {
        ui.add_space(4.0);
        if ui.add(egui::Button::new(RichText::new("Reset to original").color(Color32::from_gray(200))).frame(false)).clicked() {
            edit.output = Output::default();
        }
    }
    edit.output != before
}

fn mbps(kbps: u32) -> String {
    if kbps >= 10_000 { format!("{:.0} Mbps", kbps as f32 / 1000.0) } else { format!("{:.1} Mbps", kbps as f32 / 1000.0) }
}

fn human_bytes(b: u64) -> String {
    let mb = b as f64 / 1024.0 / 1024.0;
    if mb >= 1024.0 { format!("{:.1} GB", mb / 1024.0) } else if mb >= 10.0 { format!("{mb:.0} MB") } else { format!("{mb:.1} MB") }
}
