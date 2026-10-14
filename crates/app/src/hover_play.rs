//! A clip plays in its card while the pointer rests on it: after a moment
//! (passing over a card doesn't start anything), from its start, at its own
//! frame rate, silently, round and round. One at a time; leaving the card
//! stops it.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use crate::player::Player;

/// How long the pointer rests on a card before it plays.
const DELAY: Duration = Duration::from_millis(400);

pub struct HoverPlay {
    clip: PathBuf,
    since: Instant,
    state: State,
}

enum State {
    Waiting,
    /// Reading the clip (what it is: size, length, frame rate).
    Opening(Receiver<Option<media::ClipInfo>>),
    Playing(Box<Player>, f64),
    /// Couldn't be read: the thumbnail stays.
    Failed,
}

impl HoverPlay {
    /// The pointer is on `clip`'s card this frame.
    pub fn hovering(slot: &mut Option<HoverPlay>, clip: &Path) {
        if slot.as_ref().is_none_or(|h| h.clip != clip) {
            *slot = Some(HoverPlay { clip: clip.to_path_buf(), since: Instant::now(), state: State::Waiting });
        }
    }

    pub fn clip(&self) -> &Path {
        &self.clip
    }

    /// Move it along: start once the pointer has rested long enough, take
    /// the next frame, go back to the start at the end. Returns the picture's
    /// size once there is one to draw.
    pub fn update(&mut self, ctx: &egui::Context) -> Option<egui::Vec2> {
        match &mut self.state {
            State::Waiting => {
                if self.since.elapsed() < DELAY {
                    ctx.request_repaint_after(DELAY - self.since.elapsed());
                    return None;
                }
                let (tx, rx) = mpsc::channel();
                let clip = self.clip.clone();
                let repaint = ctx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(media::probe(&clip).ok());
                    repaint.request_repaint();
                });
                self.state = State::Opening(rx);
                None
            }
            State::Opening(rx) => {
                match rx.try_recv() {
                    Ok(Some(info)) => {
                        let duration = info.duration;
                        // No sound: the clock runs on, silent.
                        let mut player = Player::new(ctx, &self.clip, info, media::pcm::Pcm::from_tracks(Vec::new()), Vec::new());
                        player.play(duration);
                        self.state = State::Playing(Box::new(player), duration);
                    }
                    Ok(None) | Err(mpsc::TryRecvError::Disconnected) => self.state = State::Failed,
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                None
            }
            State::Playing(player, duration) => {
                // At the end: from the start again.
                if !player.is_playing() || player.time() >= *duration - 0.05 {
                    player.seek(0.0);
                    player.play(*duration);
                }
                player.update(ctx, false)
            }
            State::Failed => None,
        }
    }

    /// Draw the playing picture into `rect` (filling it, cropped to its
    /// shape); false if there's nothing to draw yet.
    pub fn paint(&self, ui: &mut egui::Ui, rect: egui::Rect, size: egui::Vec2) -> bool {
        let State::Playing(player, _) = &self.state else { return false };
        // Cover the card: the picture scaled to fill it, what's past it cut off.
        let scale = (rect.width() / size.x).max(rect.height() / size.y);
        let r = egui::Rect::from_center_size(rect.center(), size * scale);
        let mut clipped = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        clipped.set_clip_rect(rect.intersect(ui.clip_rect()));
        player.paint(&clipped, r);
        true
    }
}
