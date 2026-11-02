//! The editor's export settings: how the saved clip is made, when it
//! shouldn't be just like the recording. A part of the picture (a crop),
//! smaller (resolution, frame rate, quality), small enough to send (fit
//! under a size), or with only the mix for audio. Saved with the edit, so
//! they come back when the clip is opened again.
//!
//! Its inputs follow ui_kit's rules, as Settings' do: Resolution, Frame
//! rate and Quality are picked from a menu (Original, Auto, the common
//! ones, the levels), and Custom… puts a box beside it for a number of your
//! own; Fit under is always a number, typed straight in. The resolution and
//! frame rate menus are shared with Free up space's "Make them smaller"
//! (`free_up.rs`).

use media::{BlendShape, ClipInfo, Crop, Edit, Output};

use crate::ui_kit::{self, Choice, ChoiceField, Chosen, Custom, Preset, RATIOS, ValueField, common_ratio};

/// Common heights, offered when smaller than the picture.
pub(crate) const HEIGHTS: [u32; 6] = [2160, 1440, 1080, 720, 480, 360];
/// Common frame rates, offered when slower than the clip.
pub(crate) const RATES: [u32; 4] = [120, 60, 30, 24];
/// Common sizes to fit under, in MB.
const SIZES: [u32; 5] = [10, 25, 50, 100, 500];
/// The smallest height that can be typed.
const MIN_HEIGHT: u32 = 144;
/// Quality levels, as Settings has them for recording: a share of
/// Balanced, about 0.12 bits a pixel each frame (looks good in fast game
/// footage). Their name, share, and what they're like.
pub(crate) const LEVELS: [(&str, f64, &str); 3] = [("High", 2.0, "sharpest"), ("Balanced", 1.0, "good in fast motion"), ("Smaller", 0.5, "softer")];

/// A level's bitrate for a `w`×`h` picture at `fps`, in kbps (rounded as
/// Settings rounds it).
pub(crate) fn level_kbps(w: u32, h: u32, fps: f64, share: f64) -> u32 {
    crate::settings_ui::round_mbps(w as f64 * h as f64 * fps * 0.12 * share / 1e6) * 1000
}

/// A one-line summary for the button that opens the settings: "Original",
/// or what changes ("1080p · 30 fps · under 10 MB").
pub fn summary(info: &ClipInfo, edit: &Edit) -> String {
    let o = edit.output;
    let mut parts = Vec::new();
    if let Some(c) = o.crop.and_then(|c| c.within(info.width, info.height)) {
        parts.push(common_ratio(c.w, c.h).map_or("Cropped".into(), |i| format!("{}:{} crop", RATIOS[i].0, RATIOS[i].1)));
    }
    if o.height.is_some_and(|h| h < o.picture(info).1) {
        parts.push(format!("{}p", o.height.unwrap()));
    }
    if o.fps.is_some_and(|f| (f as f64) < info.fps - 0.5) {
        parts.push(match (o.blend_fps, o.blend_shape) {
            (Some(b), BlendShape::Even) => format!("{} fps blended from {b}", o.fps.unwrap()),
            (Some(b), BlendShape::Soft) => format!("{} fps blended softly from {b}", o.fps.unwrap()),
            (None, _) => format!("{} fps", o.fps.unwrap()),
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

/// The settings, in a popup. True if anything changed. `crop_on_picture`:
/// set when the crop is to be chosen on the picture (the crop tool).
pub fn ui(ui: &mut egui::Ui, info: &ClipInfo, edit: &mut Edit, crop_on_picture: &mut bool) -> bool {
    use crate::theme::{self, W};
    let pal = theme::pal(ui);
    let before = edit.output;
    ui.set_width(340.0);
    ui.label(theme::text("Export", 14.5, W::Semibold).color(pal.text));
    ui.add_space(2.0);
    ui.label(theme::text("How the saved clip is made. Smaller files send and upload faster.", 12.5, W::Regular).color(pal.text3));
    ui.add_space(10.0);
    rows(ui, info, edit, Form { id: "export", crop: Some(crop_on_picture), more: true, size_hint: "The most the saved clip can take", cap_mb: None, size_note: None });

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
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(theme::text(picture_words(info, edit), 13.0, W::Regular).color(pal.text3));
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

/// What an edit comes out as: "1920×1080 · 60 fps · 15 Mbps".
pub(crate) fn picture_words(info: &ClipInfo, edit: &Edit) -> String {
    let o = edit.output;
    let (w, h) = o.size(info);
    let kbps = o.video_kbps_for(info, edit).map_or(String::new(), |k| format!(" · {}", mbps(k)));
    format!("{w}×{h} · {:.0} fps{kbps}", o.frame_rate(info))
}

/// Which of the settings a form shows: the editor's Export shows them all,
/// the Share with a link dialog what an upload can change.
pub(crate) struct Form<'a> {
    /// Its fields' ids begin with this: each form keeps its own (a number
    /// of your own typed in one isn't another's).
    pub id: &'a str,
    /// The crop, and where choosing it on the picture is asked for (the
    /// editor's crop tool). `None`: no crop.
    pub crop: Option<&'a mut bool>,
    /// Frame blending and the sound tracks too.
    pub more: bool,
    /// What a size to fit under is, on its hover.
    pub size_hint: &'a str,
    /// The most it can take, in MB: then it always fits a size, up to
    /// this (an upload), and there's no Aim for. `None`: any.
    pub cap_mb: Option<u32>,
    /// A common size to fit under that's worth a note in its menu, and the
    /// note.
    pub size_note: Option<(u32, &'a str)>,
}

/// The settings' rows, each its name on the left and its control on the
/// right: what `edit`'s output is made as.
pub(crate) fn rows(ui: &mut egui::Ui, info: &ClipInfo, edit: &mut Edit, form: Form) {
    use crate::theme::{self, W};
    use crate::ui_kit::Item;
    let pal = theme::pal(ui);
    let before = edit.output;
    // Trim and tracks, for sizes worked out while the settings change.
    let mut dims = edit.clone();
    let tracks = edit.tracks.len();
    let o = &mut edit.output;
    // Capped: always a size, and never more than the cap.
    if let Some(cap) = form.cap_mb {
        o.max_mb = Some(o.max_mb.map_or(cap, |mb| mb.min(cap)));
    }
    let id = |what: &str| format!("{}_{what}", form.id);
    let row = |ui: &mut egui::Ui, label: &str, add: &mut dyn FnMut(&mut egui::Ui)| {
        ui.horizontal(|ui| {
            ui.set_min_height(36.0);
            ui.label(theme::text(label, 13.5, W::Regular).color(pal.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| add(ui));
        });
    };

    if let Some(crop_on_picture) = form.crop {
        row(ui, "Crop", &mut |ui| {
            let crop = o.crop.and_then(|c| c.within(info.width, info.height));
            let shown = match crop {
                None => "None".to_owned(),
                Some(c) => match common_ratio(c.w, c.h) {
                    Some(i) => format!("{}:{} ({}×{})", RATIOS[i].0, RATIOS[i].1, c.w, c.h),
                    None => format!("{}×{}", c.w, c.h),
                },
            };
            let r = ui_kit::select(ui, &shown, FIELD_W);
            ui_kit::menu_popup(&r, false, 240.0, |ui| {
                if Item::new("None").check(crop.is_none()).tip("The whole picture").show(ui).clicked() {
                    o.crop = None;
                }
                // Each shape the biggest it can be, where the crop is (else in
                // the middle); not one the picture already is.
                let centre = crop.map_or((info.width as f64 / 2.0, info.height as f64 / 2.0), |c| c.centre());
                for (i, &(w, h, note)) in RATIOS.iter().enumerate() {
                    let Some(c) = Crop::largest(info.width, info.height, w, h, centre).within(info.width, info.height) else { continue };
                    if Item::new(format!("{w}:{h}")).check(crop.and_then(|c| common_ratio(c.w, c.h)) == Some(i)).value(note).show(ui).clicked() {
                        o.crop = Some(c);
                    }
                }
                ui_kit::menu_sep(ui);
                if Item::new("Adjust on the picture…").icon(egui_phosphor::regular::CROP).keys(Some(vec!["C".into()])).tip("Drag its corners and move it over the picture").show(ui).clicked() {
                    *crop_on_picture = true;
                }
            });
        });
    }

    // The quality level chosen, if it's one: it follows the picture's size
    // and rate as they change (Balanced at 720p is less than at 1440p).
    let level_of = |o: &Output| {
        let (w, h) = o.size(info);
        let fps = o.frame_rate(info);
        o.video_kbps.and_then(|k| LEVELS.iter().position(|l| level_kbps(w, h, fps, l.1) == k))
    };
    let level = level_of(o);
    // While a size decides, Auto picks the resolution and rate (each with
    // the other as it's set), and Original keeps the clip's own.
    let sized = o.max_mb.is_some();
    let ph = o.picture(info).1;
    let src = info.fps.round() as u32;
    let original = |what: String| Choice::new("Original").shown(format!("Original ({what})")).note(if sized { format!("{what}, as it is") } else { what });
    let auto = |what: String, lower: bool| Choice::new("Auto").shown(format!("Auto ({what})")).note(if lower { format!("{what}, to fit the size") } else { what });
    row(ui, "Resolution", &mut |ui| {
        let auto_h = Output { height: None, ..*o }.size(info).1;
        let firsts = (sized.then(|| auto(format!("{auto_h}p"), auto_h < ph)), original(format!("{ph}p")));
        if let Some(h) = height_choice(ui, &id("height"), o.height, ph, o.size(info).1, firsts, "any height") {
            o.height = h;
        }
    });
    row(ui, "Frame rate", &mut |ui| {
        let auto_f = Output { fps: None, ..*o }.frame_rate(info).round() as u32;
        let firsts = (sized.then(|| auto(format!("{auto_f} fps"), auto_f < src)), original(format!("{src} fps")));
        if let Some(f) = fps_choice(ui, &id("fps"), o.fps, src, o.frame_rate(info).round() as u32, firsts, "any frame rate") {
            o.fps = f;
        }
    });

    let out = o.frame_rate(info).round() as u32;
    let rates = if out < src && form.more { media::blend_rates(out, info.fps) } else { Vec::new() };
    if !rates.is_empty() {
        row(ui, "Frame blending", &mut |ui| {
            let shown = match o.blend_fps.filter(|b| rates.contains(b)) {
                Some(b) => format!("From {b} fps"),
                None => "Off".to_owned(),
            };
            let r = ui_kit::select(ui, &shown, FIELD_W);
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
        if o.blend_fps.is_some_and(|b| rates.contains(&b)) {
            row(ui, "Blend", &mut |ui| {
                use crate::settings_ui::{BLEND_EVEN_TIP, BLEND_SOFT_TIP};
                let opts = [ui_kit::Seg { icon: None, label: "Even", tip: Some(BLEND_EVEN_TIP) }, ui_kit::Seg { icon: None, label: "Soft", tip: Some(BLEND_SOFT_TIP) }];
                let chosen = usize::from(o.blend_shape == BlendShape::Soft);
                if let Some(i) = ui_kit::segmented_opts(ui, id("blend_shape"), &opts, chosen, None, true) {
                    o.blend_shape = if i == 0 { BlendShape::Even } else { BlendShape::Soft };
                }
            });
        }
    }
    // A level picked follows the new size or rate.
    if let Some(i) = level.filter(|_| o.height != before.height || o.fps != before.fps) {
        let (w, h) = o.size(info);
        o.video_kbps = Some(level_kbps(w, h, o.frame_rate(info), LEVELS[i].1));
    }

    // Either the quality decides how big it comes out, or a size does (and
    // then the quality, and the resolution and rate left on Auto, follow).
    // Capped, it's always a size.
    if form.cap_mb.is_none() {
        row(ui, "Aim for", &mut |ui| {
            let opts = [
                ui_kit::Seg { icon: None, label: "Quality", tip: Some("Choose how good it looks: the size follows") },
                ui_kit::Seg { icon: None, label: "File size", tip: Some("Choose the most it can take, for where there's a limit: the quality follows") },
            ];
            if let Some(i) = ui_kit::segmented_opts(ui, id("aim"), &opts, usize::from(o.max_mb.is_some()), None, true) {
                o.max_mb = if i == 1 {
                    // A common size under what it would come to now.
                    dims.output = *o;
                    let now = o.estimate_bytes(info, &dims).map(|b| (b / 1024 / 1024) as u32);
                    Some(match now {
                        Some(mb) => SIZES.iter().rev().copied().find(|&s| s < mb).unwrap_or((mb / 2).max(1)),
                        None => 25,
                    })
                } else {
                    None
                };
            }
        });
    }
    let most = form.cap_mb.unwrap_or(MAX_MB);
    match o.max_mb {
        // Always a number: typed straight in, the common sizes under its ∨.
        Some(mb) => row(ui, "Fit under", &mut |ui| {
            let presets = SIZES
                .iter()
                .filter(|&&s| s <= most)
                .map(|&s| {
                    let p = Preset::new(format!("{s} MB"), s as f64);
                    match form.size_note {
                        Some((at, note)) if at == s => p.note(note),
                        _ => p,
                    }
                })
                .collect();
            let field = ValueField { id: egui::Id::new(id("size")), value: mb as f64, decimals: 0, range: 1.0..=most as f64, unit: "MB", width: FIELD_W, hint: form.size_hint, presets };
            if let Some(v) = field.show(ui) {
                o.max_mb = Some(v as u32);
            }
        }),
        None => row(ui, "Quality", &mut |ui| {
            let (w, h) = o.size(info);
            let fps = o.frame_rate(info);
            // As the recording, scaled down with a smaller picture.
            dims.output = Output { video_kbps: None, ..*o };
            let own = o.video_kbps_for(info, &dims).or(info.video_kbps);
            // Only levels below what it has.
            let levels: Vec<(&str, u32)> = LEVELS.iter().map(|&(n, share, _)| (n, level_kbps(w, h, fps, share))).filter(|&(_, k)| own.is_none_or(|s| k < s)).collect();
            if let Some(k) = quality_choice(ui, &id("kbps"), o.video_kbps, own, &levels) {
                o.video_kbps = k;
            }
        }),
    }

    if tracks > 1 && form.more {
        row(ui, "Sound", &mut |ui| {
            let opts = [
                ui_kit::Seg { icon: None, label: "All tracks", tip: Some("The mix, then each source on its own track, to rebalance later") },
                ui_kit::Seg { icon: None, label: "Mix only", tip: Some("Just what you hear: smaller, and some apps only play one track anyway") },
            ];
            if let Some(i) = ui_kit::segmented_opts(ui, id("audio"), &opts, usize::from(o.mix_only), None, true) {
                o.mix_only = i == 1;
            }
        });
    }
}

// How wide the fields are.
pub(crate) const FIELD_W: f32 = 156.0;
/// The most a size to fit under can be, in MB.
const MAX_MB: u32 = 9999;

/// A resolution: Auto (`auto`: while a size to fit under picks it, saying
/// what it picks), Original (the picture's `picture_h`: kept as it is, even
/// when a size would lower it), a common height below it, or any height of
/// your own (Custom…, `custom` saying what it takes). `at`: the height it
/// comes out at now, where the box starts. The new height when it changes
/// (`None`: Auto, or Original when there's no Auto).
pub(crate) fn height_choice(ui: &mut egui::Ui, id: &str, now: Option<u32>, picture_h: u32, at: u32, (auto, original): (Option<Choice>, Choice), custom: &str) -> Option<Option<u32>> {
    let heights: Vec<u32> = HEIGHTS.into_iter().filter(|&h| h < picture_h).collect();
    let has_auto = auto.is_some();
    let mut choices: Vec<Choice> = auto.into_iter().chain([original]).collect();
    choices.extend(heights.iter().map(|h| Choice::new(format!("{h}p"))));
    let low = MIN_HEIGHT.min(picture_h);
    let field = ChoiceField { id, choices, custom: Some(Custom { unit: "p", range: low..=picture_h, note: custom }), width: FIELD_W };
    let chosen = field.show(ui, choice_of(now, &heights, picture_h, has_auto), now.filter(|&h| h < picture_h).unwrap_or(at))?;
    // Video sides are even.
    Some(picked(chosen, &heights, picture_h, has_auto, |h| h / 2 * 2))
}

/// A frame rate, as [`height_choice`] a resolution: the clip's own is `src`.
pub(crate) fn fps_choice(ui: &mut egui::Ui, id: &str, now: Option<u32>, src: u32, at: u32, (auto, original): (Option<Choice>, Choice), custom: &str) -> Option<Option<u32>> {
    let rates: Vec<u32> = RATES.into_iter().filter(|&f| f < src).collect();
    let has_auto = auto.is_some();
    let mut choices: Vec<Choice> = auto.into_iter().chain([original]).collect();
    choices.extend(rates.iter().map(|f| Choice::new(format!("{f} fps"))));
    let field = ChoiceField { id, choices, custom: Some(Custom { unit: "fps", range: 1..=src.max(1), note: custom }), width: FIELD_W };
    let chosen = field.show(ui, choice_of(now, &rates, src, has_auto), now.filter(|&f| f < src).unwrap_or(at))?;
    Some(picked(chosen, &rates, src, has_auto, |f| f))
}

/// The quality: Original (the recording's, `own` kbps), one of `levels`
/// (name and kbps), or any bitrate of your own, in whole Mbps as Settings
/// takes it. The new bitrate when it changes (`None`: Original).
fn quality_choice(ui: &mut egui::Ui, id: &str, now: Option<u32>, own: Option<u32>, levels: &[(&str, u32)]) -> Option<Option<u32>> {
    let mut choices = vec![Choice::new("Original").shown(own.map_or("Original".to_owned(), |k| format!("Original ({})", mbps(k)))).note(own.map_or(String::new(), mbps))];
    choices.extend(levels.iter().map(|&(n, k)| Choice::new(n).shown(format!("{n} ({})", mbps(k))).note(mbps(k))));
    let kbps: Vec<u32> = levels.iter().map(|l| l.1).collect();
    let field = ChoiceField { id, choices, custom: Some(Custom { unit: "Mbps", range: 1..=300, note: "any bitrate" }), width: FIELD_W };
    let at = now.or(own).map_or(8, |k| ((k as f64 / 1000.0).round() as u32).max(1));
    match field.show(ui, choice_of(now, &kbps, u32::MAX, false), at)? {
        Chosen::Choice(0) => Some(None),
        Chosen::Choice(i) => Some(kbps.get(i - 1).copied()),
        Chosen::Number(m) => Some(Some(m * 1000)),
    }
}

/// Which of a [`ChoiceField`]'s choices `now` is: none is the first (Auto,
/// or Original with no Auto); the clip's own (`top` or more) is Original;
/// then each of `common`. `None` when it's a number of your own.
fn choice_of(now: Option<u32>, common: &[u32], top: u32, auto: bool) -> Option<usize> {
    match now {
        None => Some(0),
        Some(v) if v >= top => Some(usize::from(auto)),
        Some(v) => common.iter().position(|&c| c == v).map(|i| i + 1 + usize::from(auto)),
    }
}

/// What's chosen, as a value: the first choice is none (Auto, or Original
/// with no Auto). With Auto, Original (or a number of your own at `top` or
/// more) is `top`: the clip's own, kept; with none, it's none. Else a
/// common one, or a number of your own made to fit (`fit`).
fn picked(chosen: Chosen, common: &[u32], top: u32, auto: bool, fit: impl Fn(u32) -> u32) -> Option<u32> {
    let original = auto.then_some(top);
    match chosen {
        Chosen::Choice(0) => None,
        Chosen::Choice(1) if auto => original,
        Chosen::Choice(i) => common.get(i - 1 - usize::from(auto)).copied(),
        Chosen::Number(v) if v >= top => original,
        Chosen::Number(v) => Some(fit(v)),
    }
}

pub(crate) fn mbps(kbps: u32) -> String {
    format!("{} Mbps", mbps_number(kbps))
}

/// A bitrate in Mbps, without its unit: "8", "8.5", "12".
pub(crate) fn mbps_number(kbps: u32) -> String {
    let m = kbps as f64 / 1000.0;
    if m >= 10.0 || (m - m.round()).abs() < 0.05 { format!("{:.0}", m) } else { format!("{m:.1}") }
}

pub(crate) fn human_bytes(b: u64) -> String {
    let mb = b as f64 / 1024.0 / 1024.0;
    if mb >= 1024.0 { format!("{:.1} GB", mb / 1024.0) } else if mb >= 10.0 { format!("{mb:.0} MB") } else { format!("{mb:.1} MB") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// None is the first choice (Original, Auto), the common values follow
    /// it, and anything else is a number of your own.
    #[test]
    fn choices_and_numbers() {
        let heights = [1080, 720, 480];
        let even = |h: u32| h / 2 * 2;
        // No size: Original, then the common ones.
        assert_eq!(choice_of(None, &heights, 1440, false), Some(0));
        assert_eq!(choice_of(Some(1440), &heights, 1440, false), Some(0), "the picture's own is Original");
        assert_eq!(choice_of(Some(720), &heights, 1440, false), Some(2));
        assert_eq!(choice_of(Some(900), &heights, 1440, false), None, "a height of your own");
        assert_eq!(picked(Chosen::Choice(0), &heights, 1440, false, even), None);
        assert_eq!(picked(Chosen::Choice(1), &heights, 1440, false, even), Some(1080));
        assert_eq!(picked(Chosen::Number(901), &heights, 1440, false, even), Some(900), "made even");
        assert_eq!(picked(Chosen::Number(2000), &heights, 1440, false, even), None);
        // A size decides: Auto first, then Original, which is kept.
        assert_eq!(choice_of(None, &heights, 1440, true), Some(0), "Auto");
        assert_eq!(choice_of(Some(1440), &heights, 1440, true), Some(1), "Original");
        assert_eq!(choice_of(Some(720), &heights, 1440, true), Some(3));
        assert_eq!(picked(Chosen::Choice(0), &heights, 1440, true, even), None, "Auto");
        assert_eq!(picked(Chosen::Choice(1), &heights, 1440, true, even), Some(1440), "Original, kept");
        assert_eq!(picked(Chosen::Choice(2), &heights, 1440, true, even), Some(1080));
        assert_eq!(picked(Chosen::Number(2000), &heights, 1440, true, even), Some(1440), "the picture's own, kept");
    }

    #[test]
    fn bitrates_read_well() {
        assert_eq!(mbps(8000), "8 Mbps");
        assert_eq!(mbps(8500), "8.5 Mbps");
        assert_eq!(mbps(12_400), "12 Mbps");
    }

    #[test]
    fn levels_follow_the_picture() {
        // Balanced at 1080p60 is Settings' 15 Mbps; High twice, Smaller half.
        assert_eq!(level_kbps(1920, 1080, 60.0, 1.0), 15_000);
        assert_eq!(level_kbps(1920, 1080, 60.0, 2.0), 30_000);
        assert_eq!(level_kbps(1280, 720, 30.0, 0.5), 2_000);
    }
}
