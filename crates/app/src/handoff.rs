//! What the player and the editor hand each other, switching between them
//! on one clip: the player itself (taken over where both play the same
//! file, a clip never edited), the filmstrip, and what's been read and
//! decoded of the clip's files.
//!
//! It goes from screen to screen and nowhere else. What a screen doesn't use
//! it keeps, to hand on again, so going back finds the sound already decoded
//! where you were. Leaving the clip drops it, and everything it holds with
//! it. The app drops it too when the clip's files are about to change
//! (saved, reverted, freed up), so what it holds is of the files as they are.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use media::ClipInfo;
use media::pcm::Pcm;

use crate::filmstrip::Filmstrip;
use crate::player::Player;

#[derive(Default)]
pub struct Handoff {
    /// The last screen's player: taken over if it plays the same file, else
    /// shown until the new one has a picture.
    pub player: Option<Player>,
    /// Its filmstrip, used again if it's of the same file.
    pub strip: Option<Filmstrip>,
    pub kept: Kept,
}

impl Handoff {
    /// What a screen hands on: its `player`, or the `curtain` still standing
    /// in for it while it has no picture (the next screen has one at once);
    /// its filmstrip; and what's kept.
    pub fn from_screen(player: Option<Player>, curtain: Option<Player>, strip: Filmstrip, kept: Kept) -> Self {
        let player = match curtain {
            Some(c) if c.size().is_some() && player.as_ref().is_none_or(|p| p.size().is_none()) => Some(c),
            _ => player,
        };
        Self { player, strip: Some(strip), kept }
    }

    /// The filmstrip, if it's of `source`.
    pub fn strip(&mut self, source: &Path) -> Option<Filmstrip> {
        self.strip.take().filter(|s| s.source() == source)
    }

    /// The clip couldn't be opened: the player and the filmstrip handed over
    /// go (the player would play on behind the message).
    pub fn let_go(&mut self) {
        self.player = None;
        self.strip = None;
    }
}

/// What's been read and decoded of a clip's files: each once, however often
/// you switch.
#[derive(Default)]
pub struct Kept {
    read: Vec<(PathBuf, ClipInfo)>,
    /// Audio streams (`0:a:<index>`) of a file, decoded.
    sounds: Vec<(PathBuf, Vec<usize>, Arc<Pcm>)>,
}

impl Kept {
    /// What's been read of `path`, if it has.
    pub fn info(&self, path: &Path) -> Option<&ClipInfo> {
        self.read.iter().find(|(p, _)| p == path).map(|(_, info)| info)
    }

    /// Read `path`, unless it's been read.
    pub fn probe(&mut self, path: &Path) -> anyhow::Result<ClipInfo> {
        if let Some(info) = self.info(path) {
            return Ok(info.clone());
        }
        let info = media::probe(path)?;
        self.read.push((path.to_path_buf(), info.clone()));
        Ok(info)
    }

    /// `path`'s audio streams `tracks`, from `decode` unless they've been
    /// decoded (or are being: they go on filling in all the same).
    pub fn sound(&mut self, path: &Path, tracks: &[usize], decode: impl FnOnce() -> Arc<Pcm>) -> Arc<Pcm> {
        if let Some((_, _, pcm)) = self.sounds.iter().find(|(p, t, _)| p == path && t == tracks) {
            return pcm.clone();
        }
        let pcm = decode();
        self.sounds.push((path.to_path_buf(), tracks.to_vec(), pcm.clone()));
        pcm
    }

    /// A file was renamed from `from` to `to`: what's kept of it is kept
    /// under its new name.
    pub fn renamed(&mut self, from: &Path, to: &Path) {
        let paths = self.read.iter_mut().map(|(p, _)| p).chain(self.sounds.iter_mut().map(|(p, _, _)| p));
        for p in paths.filter(|p| p.as_path() == from) {
            *p = to.to_path_buf();
        }
    }

    /// Forget every file but `path`: the others changed (a clip's original,
    /// trimmed or deleted to free up space).
    pub fn keep_only(&mut self, path: &Path) {
        self.read.retain(|(p, _)| p == path);
        self.sounds.retain(|(p, _, _)| p == path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same sound, asked for again (going back to the player, or to the
    /// editor), is the one decoded: not decoded again. Other tracks of the
    /// file (the editor's, the player's mix) are a sound of their own.
    #[test]
    fn a_sound_is_decoded_once() {
        let mut kept = Kept::default();
        let clip = Path::new("clip.mp4");
        let first = kept.sound(clip, &[0], || Pcm::from_tracks(vec![vec![0.5; 96]]));
        let again = kept.sound(clip, &[0], || panic!("decoded again"));
        assert!(Arc::ptr_eq(&first, &again));
        let tracks = kept.sound(clip, &[1, 2], || Pcm::from_tracks(vec![vec![0.1; 96], vec![0.2; 96]]));
        assert!(!Arc::ptr_eq(&first, &tracks));
        assert_eq!(tracks.len(), 2);
    }

    /// A clip renamed while it's open (from the player or the editor): what
    /// was read and decoded of it is found under its new name.
    #[test]
    fn what_is_kept_follows_a_rename() {
        let mut kept = Kept::default();
        let (from, to) = (Path::new("old.mp4"), Path::new("new.mp4"));
        let original = Path::new(".hasteclip/1/original.mp4");
        let info = ClipInfo { duration: 2.0, fps: 60.0, width: 2, height: 2, video_kbps: None, audio: Vec::new(), id: None, bt601: false };
        kept.read.push((from.to_path_buf(), info.clone()));
        kept.read.push((original.to_path_buf(), info));
        let sound = kept.sound(from, &[0], || Pcm::from_tracks(vec![vec![0.5; 96]]));
        kept.renamed(from, to);
        assert!(kept.info(from).is_none() && kept.info(to).is_some());
        assert!(kept.info(original).is_some(), "the original keeps its name");
        assert!(Arc::ptr_eq(&sound, &kept.sound(to, &[0], || panic!("decoded again"))));
    }
}
