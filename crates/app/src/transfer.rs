//! Exporting clips to a `.hesteclips` file and importing one (see
//! `archive.rs`): the dialogs, the work on a thread of its own, and a toast
//! that shows how far it's come, then how it went.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui_phosphor::regular as icon;

use crate::App;
use crate::archive::{self, Imported, Progress};

/// The export or import running, if any (one at a time).
pub struct Transfer {
    what: Kind,
    progress: Progress,
    done: std::sync::mpsc::Receiver<Result<Outcome, String>>,
}

enum Kind {
    Export(usize),
    Import,
}

enum Outcome {
    Exported(usize, PathBuf),
    Imported(Imported),
}

impl App {
    /// Ask where, then write these clips (with their edits, markers and
    /// collections) to a `.hesteclips` file.
    pub(crate) fn export_clips(&mut self, paths: &[PathBuf]) {
        if self.transfer.is_some() {
            self.toast_error("Wait for the export or import that's running to finish");
            return;
        }
        let clips: Vec<crate::clips::Clip> = self.clips.iter().filter(|c| paths.contains(&c.path)).cloned().collect();
        if clips.is_empty() {
            return;
        }
        let name = if clips.len() == 1 { clips[0].editable_name() } else { format!("{} clips", clips.len()) };
        let Some(mut out) = rfd::FileDialog::new()
            .set_title("Export clips")
            .set_file_name(format!("{name}.{}", archive::EXT))
            .add_filter("HesteClips clips", &[archive::EXT])
            .save_file()
        else {
            return;
        };
        if !out.extension().is_some_and(|e| e.eq_ignore_ascii_case(archive::EXT)) {
            let mut s = out.into_os_string();
            s.push(format!(".{}", archive::EXT));
            out = s.into();
        }
        let lib = self.settings.output_dir.clone();
        // The names of the collections each clip is in.
        let collections: Vec<Vec<String>> = clips.iter().map(|c| self.collections.list().iter().filter(|col| self.collections.contains(&col.id, &c.path)).map(|col| col.name.clone()).collect()).collect();
        let n = clips.len();
        self.start_transfer(Kind::Export(n), move |progress| archive::export(&lib, &clips, &collections, &out, progress).map(|n| Outcome::Exported(n, out)));
    }

    /// Ask for `.hesteclips` files, then import them.
    pub(crate) fn import_clips_dialog(&mut self) {
        let Some(files) = rfd::FileDialog::new().set_title("Import clips").add_filter("HesteClips clips", &[archive::EXT]).pick_files() else { return };
        self.import_clips(files);
    }

    /// Put the clips in these `.hesteclips` files into the library.
    pub(crate) fn import_clips(&mut self, files: Vec<PathBuf>) {
        if files.is_empty() {
            return;
        }
        if self.transfer.is_some() {
            self.toast_error("Wait for the export or import that's running to finish");
            return;
        }
        let lib = self.settings.output_dir.clone();
        self.start_transfer(Kind::Import, move |progress| {
            let mut all = Imported::default();
            for f in &files {
                let done = archive::import(f, &lib, progress)?;
                all.added.extend(done.added);
                all.skipped += done.skipped;
            }
            Ok(Outcome::Imported(all))
        });
    }

    fn start_transfer(&mut self, what: Kind, work: impl FnOnce(&Progress) -> Result<Outcome, String> + Send + 'static) {
        let progress = Progress::default();
        let (tx, done) = std::sync::mpsc::channel();
        let (p, ctx) = (progress.clone(), self.ctx());
        std::thread::spawn(move || {
            let _ = tx.send(work(&p));
            ctx.request_repaint();
        });
        self.transfer = Some(Transfer { what, progress, done });
        self.show_transfer();
    }

    /// `.hesteclips` files dropped on the window are imported.
    pub(crate) fn import_dropped(&mut self, ctx: &egui::Context) {
        let files: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(archive::EXT))).collect());
        self.import_clips(files);
    }

    /// Each frame: keep the toast up to date, and say how it went once done.
    pub(crate) fn pump_transfer(&mut self) {
        let Some(t) = &self.transfer else { return };
        match t.done.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.show_transfer();
                self.ctx().request_repaint_after(Duration::from_millis(100));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.transfer = None;
                self.toast_error("The export or import stopped unexpectedly");
            }
            Ok(result) => {
                let export = matches!(t.what, Kind::Export(_));
                self.transfer = None;
                match result {
                    Ok(Outcome::Exported(n, out)) => {
                        let what = if n == 1 { "1 clip".to_owned() } else { format!("{n} clips") };
                        let name = out.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        self.toast_icon(icon::EXPORT, format!("Exported {what} to “{name}”"));
                    }
                    Ok(Outcome::Imported(done)) => self.imported(done),
                    Err(e) => self.toast_error(if export { format!("Couldn't export: {e}") } else { format!("Couldn't import: {e}") }),
                }
            }
        }
    }

    /// The toast while it runs: what, and how far.
    fn show_transfer(&mut self) {
        let Some(t) = &self.transfer else { return };
        let (done, total) = (t.progress.0.load(std::sync::atomic::Ordering::Relaxed), t.progress.1.load(std::sync::atomic::Ordering::Relaxed));
        let pct = (done * 100).checked_div(total).map_or(String::new(), |p| format!(" {}%", p.min(99)));
        let text = match t.what {
            Kind::Export(1) => format!("Exporting 1 clip…{pct}"),
            Kind::Export(n) => format!("Exporting {n} clips…{pct}"),
            Kind::Import => format!("Importing clips…{pct}"),
        };
        let icon = if matches!(t.what, Kind::Export(_)) { icon::EXPORT } else { icon::DOWNLOAD_SIMPLE };
        // Kept up (refreshed) for as long as it runs.
        self.toast = Some(crate::shell::Toast { text, icon, error: false, at: Instant::now(), action: None, for_: Duration::from_secs(4) });
    }

    /// Imported: the library and its collections see the new clips.
    fn imported(&mut self, done: Imported) {
        done.collect(&mut self.collections);
        self.refresh_clips();
        let n = done.added.len();
        let skipped = match done.skipped {
            0 => String::new(),
            1 => " (1 was already here)".to_owned(),
            k => format!(" ({k} were already here)"),
        };
        match (n, done.added.first().map(|(p, _)| p)) {
            (0, _) if done.skipped > 0 => self.toast_icon(icon::CHECK_CIRCLE, "Nothing new: you already have these clips"),
            (0, _) => self.toast_icon(icon::CHECK_CIRCLE, "There were no clips in it"),
            (1, Some(p)) => {
                let title = crate::clips::title_for_stem(&p.file_stem().unwrap_or_default().to_string_lossy());
                self.toast_action(icon::DOWNLOAD_SIMPLE, format!("Imported {title}{skipped}"), "Play", crate::shell::ToastAction::Show(p.clone()));
            }
            _ => self.toast_icon(icon::DOWNLOAD_SIMPLE, format!("Imported {n} clips{skipped}")),
        }
    }
}
