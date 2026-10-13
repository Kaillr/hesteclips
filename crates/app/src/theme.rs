//! The app's design tokens: the colours of each look, the typeface and its
//! weights, the icon font, and motion. Pages draw with these, never with
//! colours of their own, so every screen reads as the same app.
//!
//! - **Colour means one thing each.** The footage is the only colourful thing
//!   on screen. Everything you can click is grey, its brightness says how
//!   important it is, and the main action is near-white (black in the light
//!   look). Green is clipping, red is recording (and destructive
//!   confirmations), amber asks for attention, and meters go green, yellow
//!   from −12 dB, red from −3 dB.
//! - **Surfaces, darkest to lightest**: video, rail, page, group, control,
//!   hover. Depth comes from tone; hairlines only separate rows in a group.
//! - **Type**: Hanken Grotesk, whose digits are all the same width, so
//!   timecodes don't jitter as they count (egui can't turn on tabular figures).
//! - **Icons**: Phosphor. Outline in controls, filled for the rail's places.
//! - **Motion**: short and eased out. Things that change size slide to it,
//!   things that appear fade and rise a little, hover fades in.

use egui::{Color32, FontFamily, FontId, Id};

/// The colours of one look.
pub struct Pal {
    pub dark: bool,
    /// The rail, behind everything.
    pub ground: Color32,
    /// Pages.
    pub base: Color32,
    /// Groups and cards on a page.
    pub raise: Color32,
    pub raise2: Color32,
    /// Buttons, fields, menus' triggers.
    pub control: Color32,
    pub control_h: Color32,
    /// A row under the pointer, a chosen item in a list.
    pub sel: Color32,
    /// The chosen option of a segmented control.
    pub seg_on: Color32,
    pub line: Color32,
    pub line2: Color32,
    pub text: Color32,
    pub text2: Color32,
    pub text3: Color32,
    /// The screen's main action.
    pub primary: Color32,
    pub primary_fg: Color32,
    pub live: Color32,
    pub rec: Color32,
    pub warn: Color32,
    pub danger: Color32,
    /// Settings that open under a switch sit in this, recessed.
    pub well: Color32,
    /// Behind video, in both looks.
    pub stage: Color32,
    /// Menus, popovers, dialogs, toasts.
    pub pop: Color32,
    /// Over the parts of a timeline that are cut away.
    pub dim: Color32,
    pub meter_bg: Color32,
    pub meter_g: Color32,
    pub meter_y: Color32,
    pub meter_r: Color32,
    pub wave: Color32,
    pub wave_hi: Color32,
    /// The library panel's chosen filter: a shade darker than its card.
    pub sb_sel: Color32,
    pub sb_hover: Color32,
    /// Behind a dialog.
    pub scrim: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// A colour at an opacity, as CSS writes it (`rgba(255,255,255,.075)`).
const fn rgba(hex: u32, alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied_const((hex >> 16) as u8, (hex >> 8) as u8, hex as u8, (alpha * 255.0 + 0.5) as u8)
}

pub const DARK: Pal = Pal {
    dark: true,
    ground: rgb(0x111215),
    base: rgb(0x17181B),
    raise: rgb(0x1E1F23),
    raise2: rgb(0x25262B),
    control: rgb(0x2A2B30),
    control_h: rgb(0x34353B),
    sel: rgba(0xFFFFFF, 0.075),
    seg_on: rgb(0x3B3C43),
    line: rgba(0xFFFFFF, 0.065),
    line2: rgba(0xFFFFFF, 0.11),
    text: rgb(0xECECEE),
    text2: rgb(0xA2A4AB),
    text3: rgb(0x6E7179),
    primary: rgb(0xF2F2F3),
    primary_fg: rgb(0x141518),
    live: rgb(0x3DD68C),
    rec: rgb(0xFF5050),
    warn: rgb(0xF2B544),
    danger: rgb(0xFF6B6B),
    well: rgba(0x000000, 0.22),
    stage: rgb(0x0A0A0B),
    pop: rgb(0x222328),
    dim: rgba(0x0A0A0C, 0.62),
    meter_bg: rgba(0xFFFFFF, 0.06),
    meter_g: rgb(0x3DD68C),
    meter_y: rgb(0xF2C94C),
    meter_r: rgb(0xFF5050),
    wave: rgba(0xAAB6D6, 0.38),
    wave_hi: rgb(0xAAB6D6),
    sb_sel: rgba(0x000000, 0.12),
    sb_hover: rgba(0xFFFFFF, 0.06),
    scrim: rgba(0x060709, 0.5),
};

pub const LIGHT: Pal = Pal {
    dark: false,
    ground: rgb(0xECEDF0),
    base: rgb(0xF8F8F9),
    raise: rgb(0xFFFFFF),
    raise2: rgb(0xF2F3F5),
    control: rgb(0xE9EAED),
    control_h: rgb(0xDFE0E4),
    sel: rgb(0xFFFFFF),
    seg_on: rgb(0xFFFFFF),
    line: rgba(0x101218, 0.08),
    line2: rgba(0x101218, 0.14),
    text: rgb(0x17181B),
    text2: rgb(0x5A5D66),
    text3: rgb(0x90939B),
    primary: rgb(0x17181B),
    primary_fg: rgb(0xFFFFFF),
    live: rgb(0x12A467),
    rec: rgb(0xE0362F),
    warn: rgb(0xB07615),
    danger: rgb(0xD23B3B),
    well: rgb(0xF3F4F6),
    stage: rgb(0x0A0A0B),
    pop: rgb(0xFFFFFF),
    dim: rgba(0xF8F8F9, 0.7),
    meter_bg: rgba(0x101218, 0.07),
    meter_g: rgb(0x12A467),
    meter_y: rgb(0xE0A82E),
    meter_r: rgb(0xE0362F),
    wave: rgba(0x465478, 0.32),
    wave_hi: rgb(0x56607A),
    sb_sel: rgba(0x101218, 0.045),
    sb_hover: rgba(0xFFFFFF, 0.4),
    scrim: rgba(0x14161E, 0.22),
};

/// The colours of the look this `Ui` is drawn in.
pub fn pal(ui: &egui::Ui) -> &'static Pal {
    of(ui.visuals())
}

pub fn of(v: &egui::Visuals) -> &'static Pal {
    if v.dark_mode { &DARK } else { &LIGHT }
}

pub fn ctx_pal(ctx: &egui::Context) -> &'static Pal {
    if ctx.global_style().visuals.dark_mode { &DARK } else { &LIGHT }
}

// --- Type ---

/// Hanken Grotesk's weights, each its own family (the font is variable: one
/// file, set to a weight per family).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum W {
    Regular,
    Medium,
    Semibold,
    Bold,
}

const MEDIUM: &str = "hanken-500";
const SEMIBOLD: &str = "hanken-600";
const BOLD: &str = "hanken-700";
/// Phosphor's filled icons (the outline ones are in every family).
const FILL: &str = "phosphor-fill";

pub fn family(w: W) -> FontFamily {
    match w {
        W::Regular => FontFamily::Proportional,
        W::Medium => FontFamily::Name(MEDIUM.into()),
        W::Semibold => FontFamily::Name(SEMIBOLD.into()),
        W::Bold => FontFamily::Name(BOLD.into()),
    }
}

pub fn font(size: f32, w: W) -> FontId {
    FontId::new(size, family(w))
}

/// Text at a size and weight.
pub fn text(s: impl Into<String>, size: f32, w: W) -> egui::RichText {
    egui::RichText::new(s).font(font(size, w))
}

/// The family of filled icons, for [`fill`] icons in a painter or `RichText`.
pub fn fill_family() -> FontFamily {
    FontFamily::Name(FILL.into())
}

/// Outline icons: `icon::SCISSORS`. They're in every text family, so they mix
/// into labels ("✂ Edit").
pub use egui_phosphor::regular as icon;

/// An icon as text at a size.
pub fn ic(name: &str, size: f32) -> egui::RichText {
    egui::RichText::new(name).font(FontId::proportional(size))
}

/// A filled icon as text at a size.
pub fn icf(name: &str, size: f32) -> egui::RichText {
    egui::RichText::new(name).font(FontId::new(size, fill_family()))
}

/// Install the typeface and icon fonts. egui's own fonts and the system's
/// symbol font stay as fallbacks for characters Hanken Grotesk lacks.
pub fn install_fonts(ctx: &egui::Context) {
    use egui::epaint::text::{FontTweak, VariationCoords};
    static HANKEN: &[u8] = include_bytes!("../assets/fonts/HankenGrotesk.ttf");
    let mut fonts = egui::FontDefinitions::default();
    let weight = |w: f32| egui::FontData::from_static(HANKEN).tweak(FontTweak { coords: VariationCoords::new([(b"wght", w)]), ..Default::default() });
    fonts.font_data.insert("hanken-400".into(), weight(400.0).into());
    fonts.font_data.insert(MEDIUM.into(), weight(500.0).into());
    fonts.font_data.insert(SEMIBOLD.into(), weight(600.0).into());
    fonts.font_data.insert(BOLD.into(), weight(700.0).into());
    fonts.font_data.insert("phosphor".into(), egui::FontData::from_static(egui_phosphor::Variant::Regular.font_bytes()).into());
    fonts.font_data.insert(FILL.into(), egui::FontData::from_static(egui_phosphor::Variant::Fill.font_bytes()).into());
    let fallbacks = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let with = |first: &str, icons: &str| {
        let mut f = vec![first.to_owned(), icons.to_owned()];
        f.extend(fallbacks.iter().cloned());
        f
    };
    fonts.families.insert(FontFamily::Proportional, with("hanken-400", "phosphor"));
    fonts.families.insert(FontFamily::Name(MEDIUM.into()), with(MEDIUM, "phosphor"));
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), with(SEMIBOLD, "phosphor"));
    fonts.families.insert(FontFamily::Name(BOLD.into()), with(BOLD, "phosphor"));
    fonts.families.insert(FontFamily::Name(FILL.into()), with("hanken-400", FILL));
    ctx.set_fonts(fonts);
}

// --- Motion ---

/// Hover and press feedback.
pub const FAST: f32 = 0.12;
/// Things moving or changing size.
pub const NORMAL: f32 = 0.2;
/// Things appearing: dialogs, toasts, panels opening.
pub const SLOW: f32 = 0.28;

/// Fast at first, settling gently: how everything in the app moves.
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

#[derive(Clone, Copy)]
struct Tween {
    from: f32,
    to: f32,
    start: f64,
    secs: f32,
}

/// `target`, reached in `secs` with [`ease_out`] whenever it changes: from
/// wherever it was, so a change mid-way carries on smoothly. The first time
/// an id is seen it's simply the target.
pub fn value(ctx: &egui::Context, id: Id, target: f32, secs: f32) -> f32 {
    let now = ctx.input(|i| i.time);
    let id = id.with("tween");
    let tween = ctx.data(|d| d.get_temp::<Tween>(id));
    let current = |tw: &Tween| {
        let t = ((now - tw.start) as f32 / tw.secs.max(1e-3)).clamp(0.0, 1.0);
        tw.from + (tw.to - tw.from) * ease_out(t)
    };
    let tw = match tween {
        None => Tween { from: target, to: target, start: now, secs },
        Some(tw) if tw.to != target => Tween { from: current(&tw), to: target, start: now, secs },
        Some(tw) => tw,
    };
    ctx.data_mut(|d| d.insert_temp(id, tw));
    let v = current(&tw);
    if (now - tw.start) < f64::from(tw.secs) {
        ctx.request_repaint();
    }
    v
}

/// 0 to 1 as `on` turns on, back as it turns off, eased.
pub fn toggle(ctx: &egui::Context, id: Id, on: bool, secs: f32) -> f32 {
    value(ctx, id, if on { 1.0 } else { 0.0 }, secs)
}

/// How far along something is that started `since` (seconds, egui time),
/// eased: for things that appear.
pub fn since(ctx: &egui::Context, start: f64, secs: f32) -> f32 {
    let t = ((ctx.input(|i| i.time) - start) as f32 / secs).clamp(0.0, 1.0);
    if t < 1.0 {
        ctx.request_repaint();
    }
    ease_out(t)
}

/// When `id` was first asked about (egui time): for things that appear.
/// Forget it with [`unborn`] once the thing is gone, so it appears again next
/// time. (Reads the time before touching the store: egui's locks don't nest.)
pub fn born(ctx: &egui::Context, id: Id) -> f64 {
    let now = ctx.input(|i| i.time);
    ctx.data_mut(|d| *d.get_temp_mut_or_insert_with(id.with("born"), || now))
}

pub fn unborn(ctx: &egui::Context, id: Id) {
    ctx.data_mut(|d| d.remove::<f64>(id.with("born")));
}

/// Between two colours.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    if t <= 0.0 {
        return a;
    }
    if t >= 1.0 {
        return b;
    }
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

/// A colour at a fraction of its opacity.
pub fn fade(c: Color32, opacity: f32) -> Color32 {
    c.gamma_multiply(opacity.clamp(0.0, 1.0))
}
