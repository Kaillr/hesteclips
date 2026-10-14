//! Lengths typed the way video editors take timecode. The field always shows
//! a clock, and digits fill it from the right, like a microwave's: "5" is
//! 0:05, "50" 0:50, "500" 5:00, so there's no colon or unit to type. A "."
//! starts the part after the seconds where there is one. ↑ ↓ step a part
//! (← → or a click choose which), as does the scroll wheel while it's being
//! typed in. Words and colons ("5 min", "2:30") are read too.

use egui::{Align, Event, EventFilter, FontId, Id, Key, Pos2, Rect, Sense, Stroke, Vec2};

use crate::theme;

/// Wheel movement (points) per step: about one notch of a mouse wheel.
/// A guess; trackpads add up to it gradually.
const WHEEL_STEP: f32 = 40.0;

/// Digits that fill the clock: up to h:mm:ss.
const MAX_DIGITS: usize = 6;

pub struct TimeField<'a> {
    pub id: Id,
    /// The value now, in seconds.
    pub secs: f64,
    /// Where ↑ ↓ and the wheel stop.
    pub min: f64,
    pub max: f64,
    /// Digits after the seconds: 0 ("2:30") or 2 ("0:12.50").
    pub decimals: u8,
    pub font: FontId,
    /// Where the clock sits in its box: left or centred.
    pub align: Align,
    pub hint: &'a str,
}

/// What happened to a [`TimeField`] this frame.
pub enum Timed {
    None,
    /// Stepped with the arrows or the wheel: apply it now.
    Stepped(f64),
    /// Typed and confirmed (Enter, Tab or clicking away).
    Typed(f64),
    /// Typed, but not a length.
    Unreadable(String),
}

/// While the field has the keyboard.
#[derive(Clone, Default)]
struct Editing {
    /// What's been typed since: digits (and a ".") fill the clock;
    /// anything else is read as words.
    typed: String,
    /// The part ↑ ↓ step: 0 seconds, 1 minutes, 2 hours.
    part: usize,
    /// Wheel movement not yet a step.
    wheel: f32,
}

/// A piece of the clock as drawn.
struct Run {
    text: String,
    /// Padding the typing hasn't reached yet.
    dim: bool,
    part: Option<usize>,
}

impl TimeField<'_> {
    /// The field, drawn in `rect` (its box is the caller's). Clicking it
    /// takes the keyboard.
    pub fn show(self, ui: &mut egui::Ui, rect: Rect) -> Timed {
        let p = theme::pal(ui);
        let key = self.id.with("editing");
        let resp = ui.interact(rect, self.id, Sense::click()).on_hover_text(self.hint).on_hover_cursor(egui::CursorIcon::Text);
        if resp.clicked() {
            resp.request_focus();
        }
        let mut ed: Option<Editing> = ui.data(|d| d.get_temp(key));
        let mut out = Timed::None;
        let focused = resp.has_focus();
        let font = self.font.clone();
        let layout = |ui: &egui::Ui, ed: Option<&Editing>| -> Vec<(Run, std::sync::Arc<egui::Galley>)> {
            self.runs(ed).into_iter().map(|r| {
                let g = ui.painter().layout_no_wrap(r.text.clone(), font.clone(), p.text);
                (r, g)
            }).collect()
        };
        let place = |runs: &[(Run, std::sync::Arc<egui::Galley>)]| -> f32 {
            let w: f32 = runs.iter().map(|(_, g)| g.size().x).sum();
            match self.align {
                Align::Center => rect.center().x - w / 2.0,
                _ => rect.left(),
            }
        };
        if focused {
            ui.memory_mut(|m| m.set_focus_lock_filter(self.id, EventFilter { tab: false, horizontal_arrows: true, vertical_arrows: true, escape: true }));
            let e = ed.get_or_insert_with(|| Editing { part: if self.secs >= 60.0 { 1 } else { 0 }, ..Default::default() });
            let parts = if self.secs >= 3600.0 { 3 } else { 2 };
            // A click picks the part clicked.
            if resp.clicked() && e.typed.is_empty()
                && let Some(pos) = resp.interact_pointer_pos()
            {
                let runs = layout(ui, None);
                let mut x = place(&runs);
                for (r, g) in &runs {
                    if let Some(part) = r.part
                        && pos.x < x + g.size().x + 2.0
                    {
                        e.part = part;
                        break;
                    }
                    x += g.size().x;
                }
            }
            let mut now = self.secs;
            let step = |e: &mut Editing, now: &mut f64, by: f64| {
                let base = if e.typed.is_empty() { *now } else { read(&e.typed).unwrap_or(*now) };
                let unit = [1.0, 60.0, 3600.0][e.part.min(2)];
                *now = (base + by * unit).clamp(self.min, self.max);
                e.typed.clear();
            };
            let events = ui.input(|i| i.events.clone());
            let mut done = false;
            for ev in events {
                match ev {
                    Event::Text(t) => {
                        for c in t.chars() {
                            type_char(&mut e.typed, c, self.decimals);
                        }
                    }
                    Event::Key { key, pressed: true, modifiers, .. } => {
                        let big = if modifiers.shift { 10.0 } else { 1.0 };
                        match key {
                            Key::Backspace => {
                                e.typed.pop();
                            }
                            Key::Enter => {
                                out = confirm(&e.typed);
                                done = true;
                            }
                            Key::Escape => done = true,
                            Key::ArrowUp => {
                                step(e, &mut now, big);
                                out = Timed::Stepped(now);
                            }
                            Key::ArrowDown => {
                                step(e, &mut now, -big);
                                out = Timed::Stepped(now);
                            }
                            Key::ArrowLeft if e.typed.is_empty() => e.part = (e.part + 1).min(parts - 1),
                            Key::ArrowRight if e.typed.is_empty() => e.part = e.part.saturating_sub(1),
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            // The wheel steps while the pointer's on it (and isn't scrolling
            // the page then).
            if resp.hovered() {
                let dy = ui.input(|i| i.smooth_scroll_delta.y);
                if dy != 0.0 {
                    ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
                    e.wheel += dy;
                    while e.wheel.abs() >= WHEEL_STEP {
                        let by = e.wheel.signum();
                        e.wheel -= by * WHEEL_STEP;
                        step(e, &mut now, f64::from(by));
                        out = Timed::Stepped(now);
                    }
                }
            }
            if done {
                ed = None;
                resp.surrender_focus();
            }
        } else if let Some(e) = ed.take() {
            // Clicked away or tabbed on: what was typed counts.
            out = confirm(&e.typed);
        }
        ui.data_mut(|d| match &ed {
            Some(e) => {
                d.insert_temp(key, e.clone());
            }
            None => d.remove::<Editing>(key),
        });

        // The clock: the chosen part marked until typing starts; typing shows
        // a caret after what's typed.
        let runs = layout(ui, ed.as_ref());
        let mut x = place(&runs);
        let painter = ui.painter_at(rect.expand(2.0));
        let chosen = ed.as_ref().filter(|e| e.typed.is_empty()).map(|e| e.part);
        if let Some(part) = chosen {
            let mut xs = x;
            let mut span: Option<(f32, f32)> = None;
            for (r, g) in &runs {
                if r.part == Some(part) && !r.text.starts_with('.') {
                    span = Some(span.map_or((xs, xs + g.size().x), |(a, _)| (a, xs + g.size().x)));
                }
                xs += g.size().x;
            }
            if let Some((a, b)) = span {
                let h = self.font.size + 4.0;
                painter.rect_filled(Rect::from_min_max(Pos2::new(a - 2.0, rect.center().y - h / 2.0), Pos2::new(b + 2.0, rect.center().y + h / 2.0)), 3, p.seg_on);
            }
        }
        for (r, g) in &runs {
            painter.galley(Pos2::new(x, rect.center().y - g.size().y / 2.0), g.clone(), if r.dim { p.text3 } else { p.text });
            x += g.size().x;
        }
        if ed.as_ref().is_some_and(|e| !e.typed.is_empty()) {
            let h = self.font.size * 0.6;
            painter.vline(x + 1.5, rect.center().y - h..=rect.center().y + h, Stroke::new(1.5, p.text));
        }
        out
    }

    /// The clock as drawn: the value, or what's being typed.
    fn runs(&self, ed: Option<&Editing>) -> Vec<Run> {
        let typed = ed.map_or("", |e| e.typed.as_str());
        if typed.is_empty() {
            return clock_runs(self.secs, self.decimals);
        }
        if !is_digits(typed) {
            return vec![Run { text: typed.to_owned(), dim: false, part: None }];
        }
        // Digits fill from the right; the zeros before them are dim.
        let (int, frac) = typed.split_once('.').map_or((typed, None), |(a, b)| (a, Some(b)));
        let padded = format!("{int:0>3}");
        let pad = padded.len() - int.len();
        let n = padded.len();
        let mut runs = Vec::new();
        for (i, c) in padded.chars().enumerate() {
            let from_right = n - 1 - i;
            // A colon before each pair of digits from the right.
            if i > 0 && from_right % 2 == 1 {
                runs.push(Run { text: ":".into(), dim: i <= pad, part: None });
            }
            runs.push(Run { text: c.to_string(), dim: i < pad, part: Some((from_right / 2).min(2)) });
        }
        if self.decimals > 0 {
            let f = frac.unwrap_or("");
            runs.push(Run { text: ".".into(), dim: frac.is_none(), part: Some(0) });
            runs.push(Run { text: f.to_owned(), dim: false, part: Some(0) });
            let rest = self.decimals as usize - f.len().min(self.decimals as usize);
            if rest > 0 {
                runs.push(Run { text: "0".repeat(rest), dim: true, part: Some(0) });
            }
        }
        runs
    }
}

/// A value as the clock shows it: "2:30", "1:02:03", "0:12.50".
fn clock_runs(secs: f64, decimals: u8) -> Vec<Run> {
    let scale = 10u64.pow(u32::from(decimals));
    // Floored, as the editor's times are (a hair added so 2.30 isn't 2.29).
    let units = (secs.max(0.0) * scale as f64 + 1e-6).floor() as u64;
    let (whole, frac) = (units / scale, units % scale);
    let (h, m, s) = (whole / 3600, whole / 60 % 60, whole % 60);
    let run = |text: String, part: Option<usize>| Run { text, dim: false, part };
    let mut runs = if h > 0 {
        vec![run(h.to_string(), Some(2)), run(":".into(), None), run(format!("{m:02}"), Some(1)), run(":".into(), None), run(format!("{s:02}"), Some(0))]
    } else {
        vec![run(m.to_string(), Some(1)), run(":".into(), None), run(format!("{s:02}"), Some(0))]
    };
    if decimals > 0 {
        runs.push(run(format!(".{frac:0width$}", width = decimals as usize), Some(0)));
    }
    runs
}

/// Only digits and at most one ".": fills the clock.
fn is_digits(t: &str) -> bool {
    t.chars().all(|c| c.is_ascii_digit() || c == '.') && t.matches('.').count() <= 1
}

/// Adds a typed character: digits up to h:mm:ss (and the decimals after a
/// "."); anything else turns it into words.
fn type_char(typed: &mut String, c: char, decimals: u8) {
    let c = if c == ',' { '.' } else { c };
    if c.is_control() || (typed.is_empty() && c.is_whitespace()) {
        return;
    }
    if is_digits(typed) {
        let (int, frac) = typed.split_once('.').map_or((typed.as_str(), None), |(a, b)| (a, Some(b)));
        match (c, frac) {
            ('0'..='9', None) if int.len() >= MAX_DIGITS => return,
            // No zeros in front: they'd only push the digits out of reach.
            ('0', None) if int.is_empty() => return,
            ('0'..='9', Some(f)) if f.len() >= decimals as usize => return,
            ('.', _) if decimals == 0 || frac.is_some() => return,
            _ => {}
        }
    }
    typed.push(c);
}

fn confirm(typed: &str) -> Timed {
    if typed.trim().is_empty() {
        return Timed::None;
    }
    match read(typed) {
        Some(v) => Timed::Typed(v),
        None => Timed::Unreadable(typed.to_owned()),
    }
}

/// What's typed, in seconds: digits fill a clock from the right ("230" is
/// 2:30, "90" is 0:90, so 1:30); anything else is read as [`parse_length`].
fn read(typed: &str) -> Option<f64> {
    let t = typed.trim();
    if !is_digits(t) {
        return parse_length(t);
    }
    let (int, frac) = t.split_once('.').map_or((t, ""), |(a, b)| (a, b));
    let int: u64 = if int.is_empty() { 0 } else { int.parse().ok()? };
    let (h, m, s) = (int / 10000, int / 100 % 100, int % 100);
    let frac = if frac.is_empty() { 0.0 } else { format!("0.{frac}").parse::<f64>().ok()? };
    Some((h * 3600 + m * 60 + s) as f64 + frac)
}

/// A length typed in words or with colons, in seconds: "90", "2:30",
/// "1:02:30", "5 min", "45s", "1h".
pub fn parse_length(text: &str) -> Option<f64> {
    let t = text.trim().to_lowercase();
    if t.contains(':') {
        let parts: Vec<&str> = t.split(':').collect();
        if parts.len() > 3 {
            return None;
        }
        return parts.iter().try_fold(0.0, |acc, p| Some(acc * 60.0 + p.trim().parse::<f64>().ok()?));
    }
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let n: f64 = digits.parse().ok()?;
    let unit = t[digits.len()..].trim();
    match unit {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => Some(n),
        "m" | "min" | "mins" | "minute" | "minutes" => Some(n * 60.0),
        "h" | "hr" | "hour" | "hours" => Some(n * 3600.0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(keys: &str, decimals: u8) -> String {
        let mut t = String::new();
        for c in keys.chars() {
            type_char(&mut t, c, decimals);
        }
        t
    }

    #[test]
    fn digits_fill_from_the_right() {
        assert_eq!(read("5"), Some(5.0));
        assert_eq!(read("50"), Some(50.0));
        assert_eq!(read("500"), Some(300.0));
        assert_eq!(read("1230"), Some(750.0));
        assert_eq!(read("10000"), Some(3600.0));
        // Past 59 carries over, as a microwave's does.
        assert_eq!(read("90"), Some(90.0));
        assert_eq!(read("12.5"), Some(12.5));
        assert_eq!(read(".25"), Some(0.25));
    }

    #[test]
    fn typing() {
        assert_eq!(typed("0500", 0), "500");
        assert_eq!(typed("1234567", 0), "123456");
        assert_eq!(typed("12.5", 0), "125");
        assert_eq!(typed("12,345", 2), "12.34");
        assert_eq!(typed("5 min", 0), "5 min");
        assert_eq!(typed("2:30", 0), "2:30");
    }

    #[test]
    fn words_and_colons() {
        assert_eq!(read("2:30"), Some(150.0));
        assert_eq!(read("1:02:30"), Some(3750.0));
        assert_eq!(read("5 min"), Some(300.0));
        assert_eq!(read("45s"), Some(45.0));
        assert_eq!(read("1h"), Some(3600.0));
        assert_eq!(read("soon"), None);
    }

    #[test]
    fn clock() {
        let text = |secs, d| clock_runs(secs, d).into_iter().map(|r| r.text).collect::<String>();
        assert_eq!(text(30.0, 0), "0:30");
        assert_eq!(text(1200.0, 0), "20:00");
        assert_eq!(text(3600.0, 0), "1:00:00");
        assert_eq!(text(22.13, 2), "0:22.13");
        assert_eq!(text(2.3, 2), "0:02.30");
    }
}
