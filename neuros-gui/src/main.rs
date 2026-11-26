#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use neuros_core::device::default_backup_dir;
use neuros_core::rawdisk::{RawDisk, list_disks};
use neuros_core::scan::normalize_rel;
use neuros_core::vfs;
use std::collections::BTreeSet;

use neuros_core::Track;
use neuros_core::dedupe::{DupGroup, Relation};
use neuros_core::device::{add_destination, config_file};
use neuros_core::overrides::{Field, Overrides};
use neuros_core::sync::{Match, SyncAction, SyncPlan};
use neuros_core::{
    AddLayout, BuildOptions, DeviceInfo, Drive, PlannedAdd, ScanItem, ScanOptions, Source, SwapMode, WriteOptions,
    WriteReport,
};

/// The app icon ("NDbB"), from assets/neuros-db.svg.
const ICON_PNG: &[u8] = include_bytes!("../assets/neuros-db.png");
/// Window app ID. On Wayland the desktop shows the icon of the `.desktop`
/// file with this name (see `install_desktop_entry`).
const APP_ID: &str = "neuros-db";

fn main() -> eframe::Result {
    #[cfg(target_os = "linux")]
    ensure_desktop_icon();
    let mut viewport = egui::ViewportBuilder::default()
        .with_app_id(APP_ID)
        .with_title(concat!("Neuros Database Builder ", env!("CARGO_PKG_VERSION")))
        .with_inner_size([1100.0, 740.0])
        .with_min_inner_size([980.0, 480.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "neuros-db",
        options,
        Box::new(|cc| {
            // No device I/O here: the window must appear first. Detection
            // and reading the database run in the background.
            let mut app = App::new(cc.egui_ctx.clone());
            if let Some(p) = std::env::args_os().nth(1).map(PathBuf::from) {
                if p.is_dir() {
                    app.set_root(p);
                } else {
                    app.mode = Mode::Raw;
                    app.raw_path = p.display().to_string();
                    app.inspect();
                }
            } else {
                app.redetect();
            }
            Ok(Box::new(app))
        }),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Neuros on its own USB port, mounted by the OS.
    Mounted,
    /// Hard disk on a USB-IDE adapter (byte-swapped), or a disk image.
    Raw,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DriveChoice {
    Auto,
    C,
    D,
}

enum Msg {
    Progress(usize, usize),
    ScanDone(Result<Vec<ScanItem>, String>),
    WriteDone(Result<WriteReport, String>),
    AddDone(Result<WriteReport, String>),
    SyncPlanned(Result<SyncPlan, String>),
    SyncDone(Result<WriteReport, String>),
    EditsSaved(Result<WriteReport, String>),
    DupesFound(Result<Vec<DupGroup>, String>),
    DupesDeleted(Result<WriteReport, String>),
    /// Background device detection finished.
    Detected(Vec<PathBuf>),
    /// Background read of the device and its database finished.
    Loaded(Result<Box<Loaded>, String>),
    TextFixed(Result<(usize, WriteReport), String>),
}

/// State of the "Find duplicates" dialog.
struct DupeDialog {
    groups: Vec<DupGroup>,
    /// Per group, per file: delete this copy.
    del: Vec<Vec<bool>>,
    other_albums: bool,
    /// Asking "really delete?".
    confirm: bool,
}

impl DupeDialog {
    fn tick_suggested(&mut self) {
        self.del = self.groups.iter().map(|g| g.files.iter().map(|f| f.relation.suggest_delete()).collect()).collect();
    }

    /// (files, bytes) ticked for deletion.
    fn ticked(&self) -> (Vec<String>, u64) {
        let mut paths = Vec::new();
        let mut bytes = 0;
        for (g, d) in self.groups.iter().zip(&self.del) {
            for (f, &x) in g.files.iter().zip(d) {
                if x {
                    paths.push(f.path.clone());
                    bytes += f.size;
                }
            }
        }
        (paths, bytes)
    }

    /// Groups where every copy is ticked.
    fn all_ticked(&self) -> usize {
        self.del.iter().filter(|d| !d.is_empty() && d.iter().all(|&x| x)).count()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    /// What is in the database on the device (editable).
    Device,
    /// Result of the last scan (what Write would store).
    Scan,
}

/// State of the "Sync from PC" dialog.
struct SyncDialog {
    pc_folder: String,
    device_dir: String,
    plan: Option<SyncPlan>,
    delete_extra: bool,
    show_unchanged: bool,
    skip_dupes: bool,
}

/// Edit panel buffers for the current selection.
#[derive(Default)]
struct EditBuf {
    for_sel: Vec<usize>,
    value: [String; 4],
    initial: [String; 4],
    multi: [bool; 4],
}

const SYNC_FOLDER_FILE: &str = "sync-folder";

/// State of the "Add music" dialog.
struct AddDialog {
    plan: Vec<PlannedAdd>,
    kind: AddKind,
    folder: String,
    music_root: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AddKind {
    ByTags,
    KeepFolders,
    OneFolder,
}

/// What `inspect` reads from the device, off the UI thread.
struct Loaded {
    info: DeviceInfo,
    top_dirs: Vec<String>,
    music: String,
    tracks: Vec<Track>,
}

struct Job {
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    label: &'static str,
}

struct App {
    ctx: egui::Context,
    /// `inspect` was asked for while another job ran.
    pending_inspect: bool,
    options_open: bool,
    /// Device tracks (indices) waiting for "really delete?".
    confirm_delete: Option<Vec<usize>>,
    mode: Mode,
    root: String,
    raw_path: String,
    swap: SwapMode,
    raw_disks: Vec<RawDisk>,
    music: String,
    top_dirs: Vec<String>,
    detected: Vec<PathBuf>,
    info: Option<DeviceInfo>,
    open_error: Option<String>,

    drive_choice: DriveChoice,
    include_recordings: bool,
    placeholders: bool,
    ignore_the: bool,
    rename_files: bool,
    backup: bool,
    artist_albums: bool,

    items: Vec<ScanItem>,
    scanned: Option<Source>,
    filter: String,
    only_warnings: bool,

    job: Option<Job>,
    progress: (usize, usize),
    confirm_write: bool,
    add: Option<AddDialog>,
    sync: Option<SyncDialog>,
    dupes: Option<DupeDialog>,
    /// Preview of the "Fix ? in text" changes.
    text_fixes: Option<Vec<neuros_core::textfix::TextChange>>,
    view: View,
    /// Tracks in the device database, and the edits being made to them.
    device_tracks: Vec<Track>,
    /// Bumped whenever `items` or `device_tracks` are replaced.
    data_gen: u64,
    device_sort: Sort,
    scan_sort: Sort,
    /// (key, rows) of the last sorted view of each table.
    device_rows_cache: Option<(u64, Vec<usize>)>,
    scan_rows_cache: Option<(u64, Vec<usize>)>,
    edits: Overrides,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    edit: EditBuf,
    log: Vec<String>,
}

/// Where per-user desktop files go (`$XDG_DATA_HOME`, else `~/.local/share`).
#[cfg(target_os = "linux")]
fn xdg_data_home() -> std::io::Result<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok_or_else(|| std::io::Error::other("HOME is not set"))
}

/// Write the icon and `neuros-db.desktop` for this user if they are missing
/// or out of date. Wayland desktops can't take a window icon from the app;
/// they show the icon of the `.desktop` file named after the app ID. With
/// `visible` false the entry is hidden from the application menu, unless it
/// was already added there. Returns the file and whether anything changed.
#[cfg(target_os = "linux")]
fn install_desktop_entry(visible: bool) -> std::io::Result<(PathBuf, bool)> {
    let data = xdg_data_home()?;
    let mut changed = false;
    let write = |path: &std::path::Path, bytes: &[u8], changed: &mut bool| -> std::io::Result<()> {
        if std::fs::read(path).ok().as_deref() != Some(bytes) {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, bytes)?;
            *changed = true;
        }
        Ok(())
    };
    let icon = data.join(format!("icons/hicolor/256x256/apps/{APP_ID}.png"));
    write(&icon, ICON_PNG, &mut changed)?;
    let path = data.join(format!("applications/{APP_ID}.desktop"));
    let shown_before = std::fs::read_to_string(&path).is_ok_and(|t| !t.lines().any(|l| l.trim() == "NoDisplay=true"));
    let hidden = if visible || shown_before { "" } else { "NoDisplay=true\n" };
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=Neuros Database Builder\nComment=Build and manage the music database of a Neuros audio player\nExec=\"{}\" %f\nIcon={}\nStartupWMClass={APP_ID}\nTerminal=false\nCategories=AudioVideo;Audio;Utility;\n{hidden}",
        std::env::current_exe()?.display(),
        // A full path, not a theme name: KWin (title bar) looks theme icons
        // up once and doesn't see icons installed after it started.
        icon.display()
    );
    write(&path, entry.as_bytes(), &mut changed)?;
    Ok((path, changed))
}

/// On Linux, make sure the desktop knows the icon before the window opens.
#[cfg(target_os = "linux")]
fn ensure_desktop_icon() {
    if let Ok((_, true)) = install_desktop_entry(false) {
        // Let KDE pick up the new entry now instead of on its next scan.
        for tool in ["kbuildsycoca6", "kbuildsycoca5"] {
            if let Ok(mut child) = std::process::Command::new(tool)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                for _ in 0..30 {
                    if child.try_wait().ok().flatten().is_some() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                break;
            }
        }
    }
}

/// Mounted filesystems that look like a Neuros.
fn detect_devices() -> Vec<PathBuf> {
    let Ok(disks) = sysinfo::Disks::new_with_refreshed_list() else { return Vec::new() };
    disks
        .iter()
        .map(|d| d.mount_point().to_path_buf())
        .filter(|m| {
            let v = vfs::StdVfs::new(m.clone());
            vfs::find_file_ci(&v, "", "sn.txt").is_some() || vfs::find_dir_ci(&v, "", "WOID_DB").is_some()
        })
        .collect()
}

impl App {
    fn new(ctx: egui::Context) -> Self {
        Self {
            ctx,
            pending_inspect: false,
            options_open: false,
            confirm_delete: None,
            mode: Mode::Mounted,
            root: String::new(),
            raw_path: String::new(),
            swap: SwapMode::Auto,
            raw_disks: Vec::new(),
            music: String::new(),
            top_dirs: Vec::new(),
            detected: Vec::new(),
            info: None,
            open_error: None,
            drive_choice: DriveChoice::Auto,
            include_recordings: true,
            placeholders: true,
            ignore_the: true,
            rename_files: true,
            backup: true,
            artist_albums: false,
            items: Vec::new(),
            scanned: None,
            filter: String::new(),
            only_warnings: false,
            job: None,
            progress: (0, 0),
            confirm_write: false,
            add: None,
            sync: None,
            dupes: None,
            text_fixes: None,
            view: View::Device,
            device_tracks: Vec::new(),
            data_gen: 0,
            device_sort: None,
            scan_sort: None,
            device_rows_cache: None,
            scan_rows_cache: None,
            edits: Overrides::default(),
            selected: BTreeSet::new(),
            anchor: None,
            edit: EditBuf::default(),
            log: vec![format!("Backups go to {}", default_backup_dir().display())],
        }
    }

    fn say(&mut self, s: impl Into<String>) {
        self.log.push(s.into());
    }

    fn source(&self) -> Option<Source> {
        match self.mode {
            Mode::Mounted if !self.root.is_empty() => Some(Source::Mounted(PathBuf::from(&self.root))),
            Mode::Raw if !self.raw_path.is_empty() => {
                Some(Source::Raw { path: PathBuf::from(&self.raw_path), swap: self.swap })
            }
            _ => None,
        }
    }

    /// Look for mounted Neuros devices in the background.
    fn redetect(&mut self) {
        if self.job.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Detected(detect_devices()));
            ctx.request_repaint();
        });
        self.job = Some(Job { rx, cancel: Arc::new(AtomicBool::new(false)), label: "Looking for the Neuros" });
    }

    fn detected(&mut self, found: Vec<PathBuf>) {
        self.detected = found;
        match self.detected.len() {
            0 => self.say("No mounted Neuros found. Plug it in and press Detect, browse to it, or switch to \"Hard disk on USB-IDE adapter\"."),
            n => {
                self.say(format!(
                    "Found {n} mounted device(s): {}",
                    self.detected.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
                ));
                if self.root.is_empty() {
                    let p = self.detected[0].clone();
                    self.set_root(p);
                }
            }
        }
    }

    fn refresh_raw_disks(&mut self) {
        self.raw_disks = list_disks();
        let usable = self.raw_disks.iter().filter(|d| d.probe.is_some()).count();
        self.say(format!("Found {} unmounted disk(s), {usable} with a FAT filesystem.", self.raw_disks.len()));
        if self.raw_disks.iter().any(|d| d.error.as_deref().is_some_and(|e| e.contains("permission"))) {
            self.say("Some disks could not be opened: raw access needs root or membership of the 'disk' group.");
        }
        if self.raw_path.is_empty()
            && let Some(d) = self.raw_disks.iter().find(|d| d.probe.as_ref().is_some_and(|p| p.swapped))
        {
            self.raw_path = d.path.display().to_string();
            self.inspect();
        }
    }

    fn set_root(&mut self, p: PathBuf) {
        self.root = p.display().to_string();
        self.inspect();
    }

    /// Read the device and its database in the background (slow over the
    /// Neuros's USB 1.1); `loaded` applies the result.
    fn inspect(&mut self) {
        self.info = None;
        self.open_error = None;
        self.items.clear();
        self.scanned = None;
        self.top_dirs.clear();
        self.device_tracks.clear();
        self.data_gen += 1;
        self.selected.clear();
        let Some(src) = self.source() else { return };
        if self.job.is_some() {
            self.pending_inspect = true;
            return;
        }
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let res = src
                .open(false)
                .map(|(fs, probe)| {
                    let info = neuros_core::inspect(fs.as_ref(), probe);
                    let mut top_dirs: Vec<String> = fs
                        .read_dir("")
                        .map(|v| v.into_iter().filter(|e| e.is_dir).map(|e| e.name).collect())
                        .unwrap_or_default();
                    top_dirs.sort_by_key(|d| d.to_lowercase());
                    let music = vfs::find_dir_ci(fs.as_ref(), "", "MUSIC").unwrap_or_default();
                    let tracks = info
                        .existing
                        .as_ref()
                        .map(|db| db.tracks().iter().map(|t| t.to_track()).collect())
                        .unwrap_or_default();
                    Box::new(Loaded { info, top_dirs, music, tracks })
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Loaded(res));
            ctx.request_repaint();
        });
        self.job = Some(Job { rx, cancel: Arc::new(AtomicBool::new(false)), label: "Reading the database" });
    }

    fn loaded(&mut self, l: Loaded) {
        let Loaded { info, top_dirs, music, tracks } = l;
        if let Some(e) = &info.existing_error {
            self.say(format!("Existing database could not be read ({e}); it will be replaced."));
        }
        if let Some(p) = &info.probe {
            self.say(format!("Opened {}", p.summary()));
        }
        self.say(match &info.existing {
            Some(_) => format!("Read the database: {} tracks.", tracks.len()),
            None => "No database on the device yet.".into(),
        });
        self.top_dirs = top_dirs;
        self.music = music;
        self.artist_albums = info.artist_albums;
        self.device_tracks = tracks;
        self.data_gen += 1;
        self.edits = info.overrides.clone();
        self.selected.clear();
        self.anchor = None;
        self.edit = EditBuf::default();
        self.view = View::Device;
        self.info = Some(info);
    }

    fn drive(&self) -> Drive {
        match self.drive_choice {
            DriveChoice::C => Drive::C,
            DriveChoice::D => Drive::D,
            DriveChoice::Auto => self.info.as_ref().map(|i| i.drive).unwrap_or(Drive::C),
        }
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        let (Some(info), Some(src)) = (&self.info, self.source()) else { return };
        let drive = self.drive();
        let existing = info.existing_recordings();
        let opts = ScanOptions {
            music_dir: normalize_rel(&self.music),
            include_recordings: self.include_recordings,
            placeholders: self.placeholders,
        };
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        let ctx = ctx.clone();
        let src2 = src.clone();
        std::thread::spawn(move || {
            let res = src2
                .open(false)
                .and_then(|(fs, _)| {
                    neuros_core::scan(fs.as_ref(), drive, &opts, &existing, &c2, |d, t| {
                        let _ = tx.send(Msg::Progress(d, t));
                        ctx.request_repaint();
                    })
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::ScanDone(res));
            ctx.request_repaint();
        });
        self.progress = (0, 0);
        self.items.clear();
        self.scanned = Some(src);
        self.job = Some(Job { rx, cancel, label: "Scanning" });
        let where_ = if self.music.is_empty() { "whole device".to_string() } else { self.music.clone() };
        self.say(format!("Scanning {where_} (paths as {})", drive.prefix()));
    }

    fn start_write(&mut self, ctx: &egui::Context) {
        let Some(src) = self.source() else { return };
        let items = self.items.clone();
        let opts = WriteOptions {
            build: BuildOptions { ignore_leading_the: self.ignore_the, artist_albums: self.artist_albums },
            rename_files: self.rename_files,
            backup_dir: self.backup.then(default_backup_dir),
            drive: Some(self.drive()),
            dry_run: false,
        };
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = (|| {
                let (fs, probe) = src.open(true)?;
                let info = neuros_core::inspect(fs.as_ref(), probe);
                let rep = neuros_core::write_database(fs.as_ref(), &info, &items, &opts)?;
                fs.finish()?;
                Ok::<_, neuros_core::Error>(rep)
            })()
            .map_err(|e| e.to_string());
            let _ = tx.send(Msg::WriteDone(res));
            ctx.request_repaint();
        });
        self.job = Some(Job { rx, cancel: Arc::new(AtomicBool::new(false)), label: "Writing database" });
        self.say("Writing database...");
    }

    fn poll(&mut self) {
        let Some(job) = &self.job else { return };
        let msgs: Vec<Msg> = job.rx.try_iter().collect();
        for m in msgs {
            match m {
                Msg::Progress(d, t) => self.progress = (d, t),
                Msg::ScanDone(r) => {
                    self.job = None;
                    match r {
                        Ok(items) => {
                            let warn = items.iter().filter(|i| !i.warnings.is_empty()).count();
                            self.say(format!("Scan finished: {} files, {warn} with warnings.", items.len()));
                            let mut items = items;
                            for it in &mut items {
                                self.edits.apply(&mut it.track);
                            }
                            self.items = items;
                            self.data_gen += 1;
                            self.view = View::Scan;
                        }
                        Err(e) => {
                            self.scanned = None;
                            self.say(format!("Scan failed: {e}"));
                        }
                    }
                }
                Msg::SyncPlanned(r) => {
                    self.job = None;
                    match r {
                        Ok(plan) => {
                            let (n, nb) = plan.count(SyncAction::Copy);
                            let (c, cb) = plan.count(SyncAction::Update);
                            self.say(format!(
                                "Compared: {n} new ({}), {c} changed ({}), {} unchanged, {} only on the Neuros.",
                                fmt_mb(nb),
                                fmt_mb(cb),
                                plan.count(SyncAction::Unchanged).0,
                                plan.extra.len()
                            ));
                            if let Some(d) = &mut self.sync {
                                d.plan = Some(plan);
                            }
                        }
                        Err(e) => self.say(format!("Compare failed: {e}")),
                    }
                }
                Msg::Detected(found) => {
                    self.job = None;
                    self.detected(found);
                }
                Msg::Loaded(r) => {
                    self.job = None;
                    match r {
                        Ok(l) => self.loaded(*l),
                        Err(e) => self.open_error = Some(e),
                    }
                }
                Msg::DupesFound(r) => {
                    self.job = None;
                    match r {
                        Ok(groups) => {
                            let extra: usize = groups.iter().map(|g| g.files.len() - 1).sum();
                            self.say(format!("Found {} songs with {extra} extra copies.", groups.len()));
                            if let Some(d) = &mut self.dupes {
                                d.groups = groups;
                                d.tick_suggested();
                            }
                        }
                        Err(e) => {
                            self.say(format!("Finding duplicates failed: {e}"));
                            self.dupes = None;
                        }
                    }
                }
                Msg::TextFixed(r) => {
                    self.job = None;
                    match r {
                        Ok((n, rep)) => {
                            if let Some(b) = &rep.backup {
                                self.say(format!("Backed up old database to {}", b.display()));
                            }
                            self.say(format!("Fixed {n} values; database now has {} tracks.", rep.tracks_written));
                        }
                        Err(e) => self.say(format!("Fixing text failed: {e}")),
                    }
                    self.inspect();
                }
                Msg::DupesDeleted(r) => {
                    self.job = None;
                    match r {
                        Ok(rep) => {
                            if let Some(b) = &rep.backup {
                                self.say(format!("Backed up old database to {}", b.display()));
                            }
                            for f in &rep.deleted {
                                self.say(format!("Deleted {f}"));
                            }
                            for (f, why) in &rep.skipped {
                                self.say(format!("{f}: {why}"));
                            }
                            self.say(format!(
                                "Deleted {} files; database now has {} tracks.",
                                rep.deleted.len(),
                                rep.tracks_written
                            ));
                        }
                        Err(e) => self.say(format!("Deleting failed: {e}")),
                    }
                    self.inspect();
                }
                Msg::SyncDone(r) => {
                    self.job = None;
                    match r {
                        Ok(rep) => {
                            if let Some(b) = &rep.backup {
                                self.say(format!("Backed up old database to {}", b.display()));
                            }
                            for f in &rep.deleted {
                                self.say(format!("Removed {f}"));
                            }
                            self.say(format!(
                                "Sync done: copied {}, removed {}; database now has {} tracks.",
                                rep.added.len(),
                                rep.deleted.len(),
                                rep.tracks_written
                            ));
                            self.inspect();
                        }
                        Err(e) => {
                            self.say(format!("Sync failed: {e}"));
                            self.inspect();
                        }
                    }
                }
                Msg::EditsSaved(r) => {
                    self.job = None;
                    match r {
                        Ok(rep) => {
                            self.say(format!(
                                "Saved edits: database rewritten ({} tracks, {} with edits).",
                                rep.tracks_written, rep.edited_tracks
                            ));
                            self.inspect();
                        }
                        Err(e) => self.say(format!("Saving edits failed: {e}")),
                    }
                }
                Msg::AddDone(r) => {
                    self.job = None;
                    match r {
                        Ok(rep) => {
                            if let Some(b) = &rep.backup {
                                self.say(format!("Backed up old database to {}", b.display()));
                            }
                            for f in &rep.added {
                                self.say(format!("Copied {f}"));
                            }
                            for f in &rep.already_present {
                                self.say(format!("Already on the device: {f}"));
                            }
                            let tail = match self.mode {
                                Mode::Mounted => "Safely eject the Neuros before unplugging.",
                                Mode::Raw => "All data is flushed; you can disconnect the disk.",
                            };
                            self.say(format!(
                                "Done: added {} file(s); database now has {} tracks. {tail}",
                                rep.added.len(),
                                rep.tracks_written
                            ));
                            self.inspect();
                        }
                        Err(e) => {
                            self.say(format!("Adding failed: {e}"));
                            self.inspect();
                        }
                    }
                }
                Msg::WriteDone(r) => {
                    self.job = None;
                    match r {
                        Ok(rep) => {
                            if let Some(b) = &rep.backup {
                                self.say(format!("Backed up old database to {}", b.display()));
                            }
                            for (f, t) in &rep.renamed {
                                self.say(format!("Renamed {f} → {t}"));
                            }
                            for (p, why) in &rep.skipped {
                                self.say(format!("Skipped {p} ({why})"));
                            }
                            for d in &rep.created_dbs {
                                self.say(format!("Created empty {d} database"));
                            }
                            let tail = match self.mode {
                                Mode::Mounted => "Safely eject the Neuros before unplugging.",
                                Mode::Raw => "All data is flushed; you can disconnect the disk.",
                            };
                            self.say(format!("Done: wrote {} tracks. {tail}", rep.tracks_written));
                            self.inspect();
                        }
                        Err(e) => self.say(format!("Write failed: {e}")),
                    }
                }
            }
        }
        // An inspect asked for while busy runs once the job is done.
        if self.job.is_none() && self.pending_inspect {
            self.pending_inspect = false;
            self.inspect();
        }
    }

    /// Top area: where the Neuros is, what it is, and what to do with it.
    fn device_panel(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy();
        ui.add_enabled_ui(!busy, |ui| self.source_row(ui));
        ui.add_space(2.0);
        self.facts_row(ui);
        ui.separator();
        ui.horizontal(|ui| {
            let can_add = !busy && self.info.is_some();
            let has_db = self.info.as_ref().is_some_and(|i| i.existing.is_some());
            if ui
                .add_enabled(can_add, egui::Button::new("➕ Add files…"))
                .on_hover_text("Copy MP3/Ogg files onto the Neuros and add them to the database. You can also drop files or folders onto the window.")
                .clicked()
                && let Some(files) = pick_audio_files()
            {
                self.open_add(files);
            }
            if ui
                .add_enabled(can_add, egui::Button::new("📁 Add folders…"))
                .on_hover_text("Copy every MP3/Ogg file in the chosen folders, subfolders included.")
                .clicked()
                && let Some(dirs) = pick_music_folders()
            {
                self.open_add(dirs);
            }
            if ui
                .add_enabled(can_add, egui::Button::new("⇄ Sync from PC…"))
                .on_hover_text("Make a folder on the Neuros match a music folder on this computer.")
                .clicked()
            {
                let pc_folder =
                    std::fs::read_to_string(config_file(SYNC_FOLDER_FILE)).map(|s| s.trim().to_string()).unwrap_or_default();
                self.sync = Some(SyncDialog {
                    pc_folder,
                    device_dir: self.music_root(),
                    plan: None,
                    delete_extra: false,
                    show_unchanged: false,
                    skip_dupes: true,
                });
            }
            ui.separator();
            if ui
                .add_enabled(can_add && has_db, egui::Button::new("🧹 Duplicates…"))
                .on_hover_text("Find songs that are on the Neuros more than once and remove the extra copies.")
                .clicked()
            {
                self.dupes = Some(DupeDialog { groups: Vec::new(), del: Vec::new(), other_albums: false, confirm: false });
                let ctx = ui.ctx().clone();
                self.start_find_dupes(&ctx);
            }
            if ui
                .add_enabled(can_add && has_db, egui::Button::new("✒ Fix \"?\"…"))
                .on_hover_text("Old taggers saved apostrophes and dashes as \"?\" (Don?t, 1990?1999). Repair them in the database.")
                .clicked()
            {
                let changes = neuros_core::textfix::plan(&self.device_tracks);
                if changes.is_empty() {
                    self.say("No \"?\" to fix in the database.");
                } else {
                    self.text_fixes = Some(changes);
                }
            }
            ui.separator();
            if ui
                .add_enabled(can_add, egui::Button::new("🔍 Scan device"))
                .on_hover_text("Read every MP3/Ogg file on the Neuros. Then Write builds a fresh database from them.")
                .clicked()
            {
                let ctx = ui.ctx().clone();
                self.start_scan(&ctx);
            }
            let fresh = self.scanned.is_some() && self.scanned == self.source();
            let can_write = !busy && fresh && !self.items.is_empty();
            if ui
                .add_enabled(can_write, egui::Button::new("💾 Write database"))
                .on_hover_text("Replace the database on the Neuros with the scan results.")
                .on_disabled_hover_text("Scan the device first.")
                .clicked()
            {
                self.confirm_write = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.toggle_value(&mut self.options_open, "⚙ Options");
            });
        });
        ui.add_space(2.0);
    }

    /// Connection type and device.
    fn source_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let before = self.mode;
            egui::ComboBox::from_id_salt("mode")
                .width(210.0)
                .selected_text(match self.mode {
                    Mode::Mounted => "Neuros over USB",
                    Mode::Raw => "Hard disk on USB-IDE adapter",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.mode, Mode::Mounted, "Neuros over USB")
                        .on_hover_text("The Neuros on its own USB port, mounted by the computer.");
                    ui.selectable_value(&mut self.mode, Mode::Raw, "Hard disk on USB-IDE adapter")
                        .on_hover_text("The Neuros hard disk connected directly, or a disk image. Its contents are byte-swapped, so the computer can't mount it; this reads and writes it directly.");
                });
            if self.mode != before {
                if self.mode == Mode::Raw && self.raw_disks.is_empty() {
                    self.refresh_raw_disks();
                }
                self.inspect();
            }
            match self.mode {
                Mode::Mounted => {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.root)
                            .desired_width(360.0)
                            .hint_text("mount point, e.g. /run/media/you/NEUROS"),
                    );
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        self.inspect();
                    }
                    if ui.button("Browse…").clicked()
                        && let Some(p) = rfd::FileDialog::new().set_title("Select the Neuros drive").pick_folder()
                    {
                        self.set_root(p);
                    }
                    if ui.button("⟳ Detect").on_hover_text("Look for a mounted Neuros").clicked() {
                        self.redetect();
                    }
                    if self.detected.len() > 1 {
                        let mut pick = None;
                        egui::ComboBox::from_id_salt("detected").selected_text("Found…").show_ui(ui, |ui| {
                            for d in &self.detected {
                                if ui.selectable_label(false, d.display().to_string()).clicked() {
                                    pick = Some(d.clone());
                                }
                            }
                        });
                        if let Some(p) = pick {
                            self.set_root(p);
                        }
                    }
                }
                Mode::Raw => {
                    let mut pick = None;
                    let sel = if self.raw_path.is_empty() { "Choose a disk…".to_string() } else { self.raw_path.clone() };
                    egui::ComboBox::from_id_salt("rawdisk").selected_text(sel).width(360.0).show_ui(ui, |ui| {
                        for d in &self.raw_disks {
                            let tag = match (&d.probe, &d.error) {
                                (Some(p), _) if p.swapped => "  ✔ Neuros (byte-swapped FAT)".to_string(),
                                (Some(p), _) => format!("  {} (normal byte order)", p.fat_type),
                                (None, Some(e)) if e.contains("permission") => "  (no permission)".into(),
                                _ => "  (no FAT filesystem)".into(),
                            };
                            let label = format!("{}  {}{tag}", d.path.display(), d.description);
                            if ui.add_enabled(d.probe.is_some(), egui::Button::selectable(false, label)).clicked() {
                                pick = Some(d.path.display().to_string());
                            }
                        }
                        if self.raw_disks.is_empty() {
                            ui.label("No unmounted disks found.");
                        }
                    });
                    if let Some(p) = pick {
                        self.raw_path = p;
                        self.inspect();
                    }
                    if ui.button("⟳ Refresh").clicked() {
                        self.refresh_raw_disks();
                    }
                    if ui.button("Image file…").clicked()
                        && let Some(p) = rfd::FileDialog::new().set_title("Disk image").pick_file()
                    {
                        self.raw_path = p.display().to_string();
                        self.inspect();
                    }
                }
            }
        });
    }

    /// One quiet line about the open device.
    fn facts_row(&mut self, ui: &mut egui::Ui) {
        if let Some(e) = &self.open_error {
            ui.colored_label(ui.visuals().error_fg_color, e);
            return;
        }
        let Some(info) = &self.info else {
            ui.weak(if self.busy() { "…" } else { "No device open." });
            return;
        };
        let mut parts = vec![];
        if let Some(sn) = &info.serial {
            parts.push(format!("Serial {sn}"));
        }
        if let Some(fw) = &info.firmware {
            parts.push(format!("firmware {}", fw.trim()));
        }
        if let Some(b) = info.total_bytes {
            parts.push(if b >= 1 << 30 {
                format!("{:.1} GB", b as f64 / (1u64 << 30) as f64)
            } else {
                format!("{} MB", b >> 20)
            });
        }
        if info.probe.as_ref().is_some_and(|p| p.swapped) {
            parts.push("byte-swapped disk".into());
        }
        parts.push(match &info.existing {
            Some(_) => format!("{} tracks in the database", self.device_tracks.len()),
            None if info.existing_error.is_some() => "database unreadable".into(),
            None => "no database yet".into(),
        });
        ui.horizontal(|ui| {
            ui.weak(parts.join("  ·  "));
            if !info.looks_like_neuros() {
                ui.colored_label(ui.visuals().warn_fg_color, "  ⚠ doesn't look like a Neuros (no sn.txt or WOID_DB)");
            }
        });
    }

    /// The Options window: settings for building the database.
    fn options_window(&mut self, ctx: &egui::Context) {
        let mut open = self.options_open;
        let busy = self.busy();
        // Same label column in every section, so the controls line up.
        const OPT_LABEL_W: f32 = 96.0;
        egui::Window::new("Options")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::RIGHT_TOP, [-10.0, 96.0])
            .show(ctx, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                ui.strong("Scanning");
                egui::Grid::new("opt_scan").num_columns(2).min_col_width(OPT_LABEL_W).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Music folder");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.music).desired_width(200.0).hint_text("(whole device)"))
                            .on_hover_text("Folder on the Neuros to scan.");
                        let mut pick = None;
                        egui::ComboBox::from_id_salt("topdirs").selected_text("…").width(40.0).show_ui(ui, |ui| {
                            if ui.selectable_label(false, "(whole device)").clicked() {
                                pick = Some(String::new());
                            }
                            for d in &self.top_dirs {
                                if ui.selectable_label(false, d).clicked() {
                                    pick = Some(d.clone());
                                }
                            }
                        });
                        if let Some(p) = pick {
                            self.music = p;
                        }
                    });
                    ui.end_row();
                    ui.label("");
                    ui.checkbox(&mut self.include_recordings, "Include recordings (WOID_RECORD)");
                    ui.end_row();
                    ui.label("");
                    ui.checkbox(&mut self.placeholders, "\"Unknown Artist\" for untagged files")
                        .on_hover_text("Untagged tracks still appear under Artists / Albums / Genres.");
                    ui.end_row();
                    ui.label("");
                    ui.checkbox(&mut self.rename_files, "Rename non-ASCII file names").on_hover_text(
                        "The Neuros locks up on file names with accented or non-Latin characters. If off, those files are left out.",
                    );
                    ui.end_row();
                });
                ui.add_space(6.0);
                ui.strong("Database");
                egui::Grid::new("opt_db").num_columns(2).min_col_width(OPT_LABEL_W).spacing([12.0, 6.0]).show(ui, |ui| {
                    let auto = self
                        .info
                        .as_ref()
                        .map(|i| format!("Auto ({}, {})", i.drive.prefix(), i.drive_reason))
                        .unwrap_or("Auto".into());
                    ui.label("Paths");
                    egui::ComboBox::from_id_salt("drive")
                        .width(260.0)
                        .selected_text(match self.drive_choice {
                            DriveChoice::Auto => auto.clone(),
                            DriveChoice::C => "C: (hard disk)".into(),
                            DriveChoice::D => "D: (flash)".into(),
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.drive_choice, DriveChoice::Auto, auto);
                            ui.selectable_value(&mut self.drive_choice, DriveChoice::C, "C: (hard disk)");
                            ui.selectable_value(&mut self.drive_choice, DriveChoice::D, "D: (flash)");
                        })
                        .response
                        .on_hover_text("The Neuros stores paths as C:/… on the hard disk and D:/… on internal flash.");
                    ui.end_row();
                    ui.label("");
                    let why = self.info.as_ref().map(|i| i.artist_albums_reason.clone()).unwrap_or_default();
                    ui.checkbox(&mut self.artist_albums, "Albums → Artists menu").on_hover_text(format!(
                        "For firmware 2.14 and newer. Suggested: {} ({why}).",
                        if self.info.as_ref().is_some_and(|i| i.artist_albums) { "on" } else { "off" }
                    ));
                    ui.end_row();
                    ui.label("");
                    ui.checkbox(&mut self.ignore_the, "Sort \"The X\" under X");
                    ui.end_row();
                    ui.label("");
                    ui.checkbox(&mut self.backup, "Back up the old database before writing");
                    ui.end_row();
                });
                ui.add_space(6.0);
                ui.strong("Hard disk on USB-IDE adapter");
                egui::Grid::new("opt_raw").num_columns(2).min_col_width(OPT_LABEL_W).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Byte order");
                    let before = self.swap;
                    egui::ComboBox::from_id_salt("swap")
                        .width(260.0)
                        .selected_text(match self.swap {
                            SwapMode::Auto => "Auto-detect",
                            SwapMode::Swapped => "Swapped (Neuros)",
                            SwapMode::Normal => "Normal",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.swap, SwapMode::Auto, "Auto-detect");
                            ui.selectable_value(&mut self.swap, SwapMode::Swapped, "Swapped (Neuros)");
                            ui.selectable_value(&mut self.swap, SwapMode::Normal, "Normal");
                        });
                    if self.swap != before && self.mode == Mode::Raw {
                        self.inspect();
                    }
                    ui.end_row();
                });
                #[cfg(target_os = "linux")]
                {
                    ui.add_space(6.0);
                    ui.strong("Desktop");
                    if ui
                        .button("Add to the application menu")
                        .on_hover_text("Shows Neuros Database Builder in your application menu.")
                        .clicked()
                    {
                        match install_desktop_entry(true) {
                            Ok((p, _)) => self.say(format!("Added menu entry {}", p.display())),
                            Err(e) => self.say(format!("Could not add the menu entry: {e}")),
                        }
                    }
                }
            });
        });
        self.options_open = open;
    }

    /// Progress of the running job.
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let Some(job) = &self.job else {
                ui.weak("Ready");
                return;
            };
            ui.spinner();
            let (d, t) = self.progress;
            if t > 0 {
                let text = if job.label == "Copying" {
                    format!("Copying {:.1} / {:.1} MB", d as f64 / 1024.0, t as f64 / 1024.0)
                } else {
                    format!("{} {d}/{t}", job.label)
                };
                ui.add(egui::ProgressBar::new(d as f32 / t as f32).desired_width(320.0).text(text));
            } else {
                ui.label(format!("{}…", job.label));
            }
            if job.label == "Scanning" && ui.button("Cancel").clicked() {
                job.cancel.store(true, Ordering::Relaxed);
            }
        });
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        let total = self.items.len();
        let warn = self.items.iter().filter(|i| !i.warnings.is_empty()).count();
        let ren = self.items.iter().filter(|i| i.needs_rename()).count();
        let secs: u64 = self.items.iter().map(|i| i.track.duration_secs as u64).sum();
        ui.horizontal(|ui| {
            ui.strong(format!("{total} tracks"));
            ui.label(format!("· {}h {:02}m", secs / 3600, secs / 60 % 60));
            if warn > 0 {
                ui.colored_label(ui.visuals().warn_fg_color, format!("· {warn} with warnings"));
            }
            if ren > 0 {
                ui.label(format!("· {ren} to {}", if self.rename_files { "rename" } else { "skip" }));
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter…").desired_width(200.0));
            ui.checkbox(&mut self.only_warnings, "Only warnings");
        });

        let rows = self.scan_rows();
        ui.style_mut().interaction.selectable_labels = false;

        let warn_color = ui.visuals().warn_fg_color;
        let drive = self.drive();
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .column(Column::exact(20.0))
            .column(Column::initial(220.0).at_least(80.0).clip(true))
            .column(Column::initial(150.0).at_least(60.0).clip(true))
            .column(Column::initial(150.0).at_least(60.0).clip(true))
            .column(Column::initial(100.0).at_least(50.0).clip(true))
            .column(Column::exact(32.0))
            .column(Column::exact(48.0))
            .column(Column::remainder().at_least(100.0).clip(true))
            .header(22.0, |mut h| {
                use SortCol::*;
                for (c, col) in [
                    ("", Mark),
                    ("Title", Title),
                    ("Artist", Artist),
                    ("Album", Album),
                    ("Genre", Genre),
                    ("#", TrackNo),
                    ("Time", Time),
                    ("Path on device", Path),
                ] {
                    h.col(|ui| sort_header(ui, c, col, &mut self.scan_sort));
                }
            })
            .body(|body| {
                body.rows(20.0, rows.len(), |mut row| {
                    let it = &self.items[rows[row.index()]];
                    let t = &it.track;
                    row.col(|ui| {
                        if !it.warnings.is_empty() {
                            ui.colored_label(warn_color, "⚠").on_hover_text(it.warnings.join("\n"));
                        } else if t.recording.is_some() {
                            ui.label("🎙").on_hover_text(t.recording.as_deref().unwrap_or(""));
                        }
                    });
                    row.col(|ui| {
                        ui.label(&t.title);
                    });
                    row.col(|ui| {
                        ui.label(t.artist.as_deref().unwrap_or(""));
                    });
                    row.col(|ui| {
                        ui.label(t.album.as_deref().unwrap_or(""));
                    });
                    row.col(|ui| {
                        ui.label(t.genre.as_deref().unwrap_or(""));
                    });
                    row.col(|ui| {
                        ui.label(t.track_no.map(|n| n.to_string()).unwrap_or_default());
                    });
                    row.col(|ui| {
                        ui.label(format!("{}:{:02}", t.duration_secs / 60, t.duration_secs % 60));
                    });
                    row.col(|ui| {
                        let shown =
                            neuros_core::scan::device_path(drive, it.rename_to.as_deref().unwrap_or(&it.rel_path));
                        ui.label(shown).on_hover_text(format!("currently {}", it.rel_path));
                    });
                });
            });
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_write {
            return;
        }
        let n_bad = self.items.iter().filter(|i| i.needs_rename()).count();
        let (n_ren, n_skip) = if self.rename_files { (n_bad, 0) } else { (0, n_bad) };
        let target = self.source().map(|s| s.describe()).unwrap_or_default();
        let resp = egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading("Write database?");
            ui.label(format!(
                "This replaces the music database on {target} with {} tracks.",
                self.items.len() - n_skip
            ));
            if self.mode == Mode::Raw {
                ui.label("The disk is written directly (no OS mount). Don't disconnect it until this finishes.");
            }
            if n_ren > 0 {
                ui.label(format!("{n_ren} files with non-ASCII names will be renamed on the device."));
            }
            if n_skip > 0 {
                ui.label(format!("{n_skip} files with non-ASCII names will be left out."));
            }
            ui.label(if self.backup {
                format!("The current database will be backed up to {} first.", default_backup_dir().display())
            } else {
                "No backup will be made.".into()
            });
            ui.add_space(8.0);
            let mut go = None;
            ui.horizontal(|ui| {
                if ui.button("Write").clicked() {
                    go = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    go = Some(false);
                }
            });
            go
        });
        match resp.inner {
            Some(true) => {
                self.confirm_write = false;
                self.start_write(ctx);
            }
            Some(false) => self.confirm_write = false,
            None if resp.should_close() => self.confirm_write = false,
            None => {}
        }
    }
}

/// Column a table is sorted by.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum SortCol {
    /// The ⚠ / ✎ / 🎙 column: marked rows first.
    Mark,
    Title,
    Artist,
    Album,
    Genre,
    TrackNo,
    Time,
    Path,
}

/// Sort column and descending flag; `None` = the database's order.
type Sort = Option<(SortCol, bool)>;

/// A clickable column header: ascending, then descending, then unsorted.
fn sort_header(ui: &mut egui::Ui, label: &str, col: SortCol, sort: &mut Sort) {
    let arrow = match *sort {
        Some((c, false)) if c == col => " ⏶",
        Some((c, true)) if c == col => " ⏷",
        _ => "",
    };
    let text = if label.is_empty() && arrow.is_empty() { "·".to_string() } else { format!("{label}{arrow}") };
    let hover = if col == SortCol::Mark {
        "Sort marked rows (⚠ ✎ 🎙) first"
    } else {
        "Click to sort; again to reverse; a third time for the original order"
    };
    if ui.add(egui::Button::new(egui::RichText::new(text).strong()).frame(false)).on_hover_text(hover).clicked() {
        *sort = match *sort {
            Some((c, false)) if c == col => Some((col, true)),
            Some((c, true)) if c == col => None,
            _ => Some((col, false)),
        };
    }
}

/// Case-insensitive comparison with numbers compared by value
/// ("2._x" before "10._x").
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Equal,
            (None, _) => return Less,
            (_, None) => return Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let nb = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let (da, db) = (&a[..na], &b[..nb]);
                let strip = |d: &[u8]| {
                    let z = d.iter().take_while(|&&c| c == b'0').count();
                    d[z..].to_vec()
                };
                let (sa, sb) = (strip(da), strip(db));
                let o = sa.len().cmp(&sb.len()).then_with(|| sa.cmp(&sb));
                if o != Equal {
                    return o;
                }
                a = &a[na..];
                b = &b[nb..];
            }
            (Some(x), Some(y)) => {
                let o = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
                if o != Equal {
                    return o;
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

/// Compare optional text with empty values last.
fn text_cmp(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    let (a, b) = (a.unwrap_or(""), b.unwrap_or(""));
    a.is_empty().cmp(&b.is_empty()).then_with(|| natural_cmp(a, b))
}

/// Sort `(row, marked, track)` by `sort`; returns the rows.  Ties fall back
/// to artist → album → track → title, so albums stay in order.
fn sorted_rows(mut keyed: Vec<(usize, bool, Track)>, (col, desc): (SortCol, bool)) -> Vec<usize> {
    use std::cmp::Ordering;
    let album_order = |a: &Track, b: &Track| {
        text_cmp(a.artist.as_deref(), b.artist.as_deref())
            .then_with(|| text_cmp(a.album.as_deref(), b.album.as_deref()))
            .then_with(|| a.track_no.unwrap_or(u32::MAX).cmp(&b.track_no.unwrap_or(u32::MAX)))
            .then_with(|| natural_cmp(&a.title, &b.title))
    };
    keyed.sort_by(|(_, ma, a), (_, mb, b)| {
        let o: Ordering = match col {
            SortCol::Mark => mb.cmp(ma),
            SortCol::Title => natural_cmp(&a.title, &b.title),
            SortCol::Artist => text_cmp(a.artist.as_deref(), b.artist.as_deref()),
            SortCol::Album => text_cmp(a.album.as_deref(), b.album.as_deref()),
            SortCol::Genre => text_cmp(a.genre.as_deref(), b.genre.as_deref()),
            SortCol::TrackNo => a.track_no.unwrap_or(u32::MAX).cmp(&b.track_no.unwrap_or(u32::MAX)),
            SortCol::Time => a.duration_secs.cmp(&b.duration_secs),
            SortCol::Path => natural_cmp(&a.device_path, &b.device_path),
        };
        let o = if desc { o.reverse() } else { o };
        o.then_with(|| album_order(a, b))
    });
    keyed.into_iter().map(|(i, _, _)| i).collect()
}

fn pick_audio_files() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new()
        .set_title("Add music to the Neuros")
        .add_filter("MP3 / Ogg Vorbis", &["mp3", "MP3", "ogg", "OGG"])
        .pick_files()
}

fn pick_music_folders() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new().set_title("Add folders of music to the Neuros (subfolders included)").pick_folders()
}

fn fmt_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

impl App {
    fn music_root(&self) -> String {
        self.top_dirs.iter().find(|d| d.eq_ignore_ascii_case("MUSIC")).cloned().unwrap_or_else(|| "MUSIC".into())
    }

    fn open_add(&mut self, paths: Vec<PathBuf>) {
        let music_root = self.music_root();
        let any_dir = paths.iter().any(|p| p.is_dir());
        let plan = neuros_core::plan_add(&paths, &AddLayout::ByTags, &music_root, self.placeholders);
        if plan.is_empty() {
            self.say("No MP3 or Ogg files in what was chosen.");
            return;
        }
        match &mut self.add {
            Some(d) => {
                // Add to the open dialog, skipping files already listed.
                for p in plan {
                    if !d.plan.iter().any(|q| q.host == p.host) {
                        d.plan.push(p);
                    }
                }
                d.update_dests();
            }
            None => {
                // Folders picked: keep their structure by default.
                let kind = if any_dir { AddKind::KeepFolders } else { AddKind::ByTags };
                let mut d = AddDialog { plan, kind, folder: music_root.clone(), music_root };
                d.update_dests();
                self.add = Some(d);
            }
        }
    }

    fn start_add(&mut self, ctx: &egui::Context, plan: Vec<PlannedAdd>) {
        let Some(src) = self.source() else { return };
        let opts = WriteOptions {
            build: BuildOptions { ignore_leading_the: self.ignore_the, artist_albums: self.artist_albums },
            rename_files: self.rename_files,
            backup_dir: self.backup.then(default_backup_dir),
            drive: Some(self.drive()),
            dry_run: false,
        };
        let fallback = ScanOptions {
            music_dir: String::new(),
            include_recordings: self.include_recordings,
            placeholders: self.placeholders,
        };
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = (|| {
                let (fs, probe) = src.open(true)?;
                let info = neuros_core::inspect(fs.as_ref(), probe);
                let rep = neuros_core::add_files(fs.as_ref(), &info, &plan, &opts, &fallback, |d, t| {
                    let _ = tx.send(Msg::Progress((d / 1024) as usize, (t / 1024) as usize));
                    ctx.request_repaint();
                })?;
                fs.finish()?;
                Ok::<_, neuros_core::Error>(rep)
            })()
            .map_err(|e| e.to_string());
            let _ = tx.send(Msg::AddDone(res));
            ctx.request_repaint();
        });
        self.progress = (0, 0);
        self.items.clear();
        self.scanned = None;
        self.job = Some(Job { rx, cancel: Arc::new(AtomicBool::new(false)), label: "Copying" });
        self.say("Copying music to the device…");
    }

    fn add_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.add else { return };
        let mut action: Option<bool> = None;
        let mut more: Option<Vec<PathBuf>> = None;
        let mut remove: Option<usize> = None;
        let top_dirs = self.top_dirs.clone();
        let mode_raw = self.mode == Mode::Raw;

        let resp = egui::Modal::new(egui::Id::unique("add")).show(ctx, |ui| {
            ui.set_width(820.0);
            ui.heading("Add music to the Neuros");
            let total: u64 = d.plan.iter().map(|p| p.size).sum();
            ui.label(format!("{} file(s), {}", d.plan.len(), fmt_mb(total)));
            ui.add_space(4.0);

            let before = (d.kind, d.folder.clone());
            ui.horizontal(|ui| {
                ui.label("Put files:");
                ui.radio_value(&mut d.kind, AddKind::ByTags, format!("sorted into {}/Artist/Album (from tags)", d.music_root));
            });
            ui.horizontal(|ui| {
                ui.add_space(62.0);
                ui.radio_value(&mut d.kind, AddKind::KeepFolders, "keeping their folders, under:")
                    .on_hover_text("A picked folder ~/Music/Some Band becomes <folder>/Some Band/… with its subfolders.");
                ui.radio_value(&mut d.kind, AddKind::OneFolder, "all in the folder:");
                let need_folder = d.kind != AddKind::ByTags;
                ui.add_enabled(need_folder, egui::TextEdit::singleline(&mut d.folder).desired_width(200.0));
                ui.add_enabled_ui(need_folder, |ui| {
                    egui::ComboBox::from_id_salt("addfolder").selected_text("…").show_ui(ui, |ui| {
                        for t in &top_dirs {
                            if ui.selectable_label(false, t).clicked() {
                                d.folder = t.clone();
                            }
                        }
                    });
                });
            });
            if before != (d.kind, d.folder.clone()) {
                d.update_dests();
            }

            ui.add_space(4.0);
            let row_h = 20.0;
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .max_scroll_height(320.0)
                .column(Column::exact(20.0))
                .column(Column::initial(180.0).clip(true))
                .column(Column::initial(120.0).clip(true))
                .column(Column::initial(120.0).clip(true))
                .column(Column::exact(48.0))
                .column(Column::remainder().at_least(150.0).clip(true))
                .column(Column::exact(24.0))
                .header(row_h, |mut h| {
                    for c in ["", "Title", "Artist", "Album", "Time", "Goes to", ""] {
                        h.col(|ui| {
                            ui.strong(c);
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_h, d.plan.len(), |mut row| {
                        let i = row.index();
                        let p = &d.plan[i];
                        row.col(|ui| {
                            if !p.tags.warnings.is_empty() {
                                ui.label("⚠").on_hover_text(p.tags.warnings.join("\n"));
                            }
                        });
                        row.col(|ui| {
                            ui.label(&p.tags.title).on_hover_text(p.host.display().to_string());
                        });
                        row.col(|ui| {
                            ui.label(p.tags.artist.as_deref().unwrap_or(""));
                        });
                        row.col(|ui| {
                            ui.label(p.tags.album.as_deref().unwrap_or(""));
                        });
                        row.col(|ui| {
                            ui.label(format!("{}:{:02}", p.tags.duration_secs / 60, p.tags.duration_secs % 60));
                        });
                        row.col(|ui| {
                            ui.label(&p.dest);
                        });
                        row.col(|ui| {
                            if ui.small_button("✖").on_hover_text("Leave this file out").clicked() {
                                remove = Some(i);
                            }
                        });
                    });
                });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("More files…").clicked() {
                    more = pick_audio_files();
                }
                if ui.button("More folders…").clicked() {
                    more = pick_music_folders();
                }
            });
            ui.add_space(4.0);
            ui.label("Files keep their tags; names are made ASCII-safe for the Neuros. The database is updated right after copying (no full rescan).");
            if mode_raw {
                ui.label("The disk is written directly. Don't disconnect it until this finishes.");
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let label = format!("Copy {} file(s) and update database", d.plan.len());
                if ui.add_enabled(!d.plan.is_empty(), egui::Button::new(label)).clicked() {
                    action = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    action = Some(false);
                }
            });
        });

        if let Some(i) = remove {
            d.plan.remove(i);
        }
        let close = action.is_some() || (action.is_none() && resp.should_close());
        if close {
            let d = self.add.take().unwrap();
            if action == Some(true) {
                self.start_add(ctx, d.plan);
            }
        } else if let Some(files) = more {
            self.open_add(files);
        }
    }
}

impl App {
    fn edits_dirty(&self) -> bool {
        self.info.as_ref().is_some_and(|i| i.overrides != self.edits)
    }

    /// Device track `i` as it will be after the pending edits.
    fn shown(&self, i: usize) -> Track {
        let mut t = self.device_tracks[i].clone();
        self.edits.apply(&mut t);
        t
    }

    fn device_table(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter…").desired_width(220.0));
            ui.label(format!("{} selected", self.selected.len()));
            if ui.button("Select all shown").clicked() {
                self.selected = self.filtered_device_rows().into_iter().collect();
            }
            if !self.selected.is_empty() && ui.button("Clear selection").clicked() {
                self.selected.clear();
            }
            ui.label("· click to select, Ctrl/Shift-click for more, then edit on the right");
        });
        let rows = self.device_rows();
        let mut clicked: Option<(usize, egui::Modifiers)> = None;
        // Text in rows must not grab clicks (for text selection); a click
        // anywhere on a row selects it.
        ui.style_mut().interaction.selectable_labels = false;
        let mods = ui.input(|i| i.modifiers);
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .column(Column::exact(20.0))
            .column(Column::initial(240.0).at_least(80.0).clip(true))
            .column(Column::initial(160.0).at_least(60.0).clip(true))
            .column(Column::initial(160.0).at_least(60.0).clip(true))
            .column(Column::initial(100.0).at_least(50.0).clip(true))
            .column(Column::exact(48.0))
            .column(Column::remainder().at_least(100.0).clip(true))
            .header(22.0, |mut h| {
                use SortCol::*;
                for (c, col) in [
                    ("", Mark),
                    ("Title", Title),
                    ("Artist", Artist),
                    ("Album", Album),
                    ("Genre", Genre),
                    ("Time", Time),
                    ("Path on device", Path),
                ] {
                    h.col(|ui| sort_header(ui, c, col, &mut self.device_sort));
                }
            })
            .body(|body| {
                body.rows(20.0, rows.len(), |mut row| {
                    let i = rows[row.index()];
                    let t = self.shown(i);
                    row.set_selected(self.selected.contains(&i));
                    let edited = self.edits.is_edited(&t.device_path);
                    row.col(|ui| {
                        if edited {
                            ui.label("✎").on_hover_text("Edited (kept across rescans)");
                        } else if t.recording.is_some() {
                            ui.label("🎙");
                        }
                    });
                    row.col(|ui| {
                        ui.label(&t.title);
                    });
                    row.col(|ui| {
                        ui.label(t.artist.as_deref().unwrap_or(""));
                    });
                    row.col(|ui| {
                        ui.label(t.album.as_deref().unwrap_or(""));
                    });
                    row.col(|ui| {
                        ui.label(t.genre.as_deref().unwrap_or(""));
                    });
                    row.col(|ui| {
                        ui.label(format!("{}:{:02}", t.duration_secs / 60, t.duration_secs % 60));
                    });
                    row.col(|ui| {
                        ui.label(&t.device_path);
                    });
                    if row.response().clicked() {
                        clicked = Some((i, mods));
                    }
                });
            });
        if !self.selected.is_empty()
            && !self.busy()
            && !self.edits_dirty()
            && self.confirm_delete.is_none()
            && ui.input(|i| i.key_pressed(egui::Key::Delete))
            && !ui.ctx().egui_wants_keyboard_input()
        {
            self.confirm_delete = Some(self.selected.iter().copied().collect());
        }
        if let Some((i, m)) = clicked {
            let range = if m.shift {
                self.anchor.and_then(|a| {
                    let pa = rows.iter().position(|&r| r == a)?;
                    let pb = rows.iter().position(|&r| r == i)?;
                    Some((pa.min(pb), pa.max(pb)))
                })
            } else {
                None
            };
            if let Some((lo, hi)) = range {
                if !m.command {
                    self.selected.clear();
                }
                self.selected.extend(rows[lo..=hi].iter().copied());
            } else if m.command {
                if !self.selected.remove(&i) {
                    self.selected.insert(i);
                }
                self.anchor = Some(i);
            } else {
                self.selected.clear();
                self.selected.insert(i);
                self.anchor = Some(i);
            }
        }
    }

    /// Filtered and sorted rows of the device table (cached).
    fn device_rows(&mut self) -> Vec<usize> {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        (self.data_gen, &self.filter, self.device_sort, self.device_tracks.len(), self.edits.to_text()).hash(&mut h);
        let key = h.finish();
        if let Some((k, rows)) = &self.device_rows_cache
            && *k == key
        {
            return rows.clone();
        }
        let mut rows = self.filtered_device_rows();
        if let Some(sort) = self.device_sort {
            let keyed: Vec<(usize, bool, Track)> = rows
                .iter()
                .map(|&i| {
                    let t = self.shown(i);
                    (i, self.edits.is_edited(&t.device_path) || t.recording.is_some(), t)
                })
                .collect();
            rows = sorted_rows(keyed, sort);
        }
        self.device_rows_cache = Some((key, rows.clone()));
        rows
    }

    /// Filtered and sorted rows of the scan table (cached).
    fn scan_rows(&mut self) -> Vec<usize> {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        (self.data_gen, &self.filter, self.only_warnings, self.scan_sort, self.items.len()).hash(&mut h);
        let key = h.finish();
        if let Some((k, rows)) = &self.scan_rows_cache
            && *k == key
        {
            return rows.clone();
        }
        let f = self.filter.to_lowercase();
        let mut rows: Vec<usize> = (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                if self.only_warnings && it.warnings.is_empty() {
                    return false;
                }
                if f.is_empty() {
                    return true;
                }
                let t = &it.track;
                [Some(&t.title), t.artist.as_ref(), t.album.as_ref(), t.genre.as_ref(), Some(&t.device_path)]
                    .into_iter()
                    .flatten()
                    .any(|s| s.to_lowercase().contains(&f))
            })
            .collect();
        if let Some(sort) = self.scan_sort {
            let keyed = rows
                .iter()
                .map(|&i| {
                    let it = &self.items[i];
                    let mut t = it.track.clone();
                    t.device_path = it.rename_to.clone().unwrap_or_else(|| it.rel_path.clone());
                    (i, !it.warnings.is_empty() || t.recording.is_some(), t)
                })
                .collect();
            rows = sorted_rows(keyed, sort);
        }
        self.scan_rows_cache = Some((key, rows.clone()));
        rows
    }

    fn filtered_device_rows(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.device_tracks.len())
            .filter(|&i| {
                if f.is_empty() {
                    return true;
                }
                let t = self.shown(i);
                [Some(&t.title), t.artist.as_ref(), t.album.as_ref(), t.genre.as_ref(), Some(&t.device_path)]
                    .into_iter()
                    .flatten()
                    .any(|s| s.to_lowercase().contains(&f))
            })
            .collect()
    }

    fn edit_panel(&mut self, ui: &mut egui::Ui) {
        let sel: Vec<usize> = self.selected.iter().copied().collect();
        if self.edit.for_sel != sel {
            // Load the selection into the edit fields.
            let shown: Vec<Track> = sel.iter().map(|&i| self.shown(i)).collect();
            let mut b = EditBuf { for_sel: sel.clone(), ..Default::default() };
            for (k, f) in Field::ALL.iter().enumerate() {
                let first = f.get(&shown[0]).unwrap_or("").to_string();
                let same = shown.iter().all(|t| f.get(t).unwrap_or("") == first);
                b.value[k] = if same { first } else { String::new() };
                b.multi[k] = !same;
            }
            b.initial = b.value.clone();
            self.edit = b;
        }
        let n = sel.len();
        ui.add_space(6.0);
        ui.heading(if n == 1 { "Edit track".to_string() } else { format!("Edit {n} tracks") });
        if n == 1 {
            ui.label(egui::RichText::new(&self.device_tracks[sel[0]].device_path).small());
        }
        ui.add_space(6.0);
        egui::Grid::new("editgrid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            for (k, f) in Field::ALL.iter().enumerate() {
                ui.label(["Title", "Artist", "Album", "Genre"][k]);
                let enabled = !(*f == Field::Title && n > 1);
                let hint = if self.edit.multi[k] { "(different values — leave empty to keep)" } else { "" };
                ui.add_enabled(
                    enabled,
                    egui::TextEdit::singleline(&mut self.edit.value[k]).hint_text(hint).desired_width(f32::INFINITY),
                );
                ui.end_row();
            }
        });
        let changed: Vec<usize> = (0..4)
            .filter(|&k| {
                self.edit.value[k] != self.edit.initial[k] && !(self.edit.multi[k] && self.edit.value[k].is_empty())
            })
            .collect();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.add_enabled(!changed.is_empty(), egui::Button::new("Apply")).clicked() {
                for &i in &sel {
                    let path = self.device_tracks[i].device_path.clone();
                    for &k in &changed {
                        if Field::ALL[k] == Field::Title && n > 1 {
                            continue;
                        }
                        self.edits.set(&path, Field::ALL[k], &self.edit.value[k]);
                    }
                }
                self.edit.for_sel.clear(); // reload from the edited values
            }
            if ui.add_enabled(!changed.is_empty(), egui::Button::new("Undo")).clicked() {
                self.edit.for_sel.clear();
            }
        });
        let any_edited = sel.iter().any(|&i| self.edits.is_edited(&self.device_tracks[i].device_path));
        if any_edited
            && ui
                .button("Forget edits")
                .on_hover_text(
                    "Remove the stored edits for these tracks. Their file tags come back with the next Scan + Write.",
                )
                .clicked()
        {
            for &i in &sel {
                self.edits.clear(&self.device_tracks[i].device_path);
            }
            self.edit.for_sel.clear();
        }
        ui.add_space(10.0);
        ui.separator();
        let dirty = self.edits_dirty();
        let label = if n == 1 {
            "🗑 Delete from Neuros…".to_string()
        } else {
            format!("🗑 Delete {n} tracks from Neuros…")
        };
        if ui
            .add_enabled(!self.busy() && !dirty, egui::Button::new(label))
            .on_hover_text("Delete the music files from the Neuros and remove them from the database. (Delete key)")
            .on_disabled_hover_text("Save or discard your edits first.")
            .clicked()
        {
            self.confirm_delete = Some(sel.clone());
        }
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new(
                "Edits change the Neuros database, not the music files. They're stored on the device and kept when you rescan, add or sync. Press \"Save edits to Neuros\" to write them.",
            )
            .small(),
        );
    }

    fn write_options(&self) -> WriteOptions {
        WriteOptions {
            build: BuildOptions { ignore_leading_the: self.ignore_the, artist_albums: self.artist_albums },
            rename_files: self.rename_files,
            backup_dir: self.backup.then(default_backup_dir),
            drive: Some(self.drive()),
            dry_run: false,
        }
    }

    fn start_save_edits(&mut self, ctx: &egui::Context) {
        let Some(src) = self.source() else { return };
        let edits = self.edits.clone();
        let opts = self.write_options();
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = (|| {
                let (fs, probe) = src.open(true)?;
                let info = neuros_core::inspect(fs.as_ref(), probe);
                let rep = neuros_core::device::save_edits(fs.as_ref(), &info, &edits, &opts)?;
                fs.finish()?;
                Ok::<_, neuros_core::Error>(rep)
            })()
            .map_err(|e| e.to_string());
            let _ = tx.send(Msg::EditsSaved(res));
            ctx.request_repaint();
        });
        self.job = Some(Job { rx, cancel: Arc::new(AtomicBool::new(false)), label: "Saving edits" });
        self.say("Saving edits…");
    }

    fn start_sync_plan(&mut self, ctx: &egui::Context) {
        let (Some(src), Some(d)) = (self.source(), &self.sync) else { return };
        let pc = PathBuf::from(d.pc_folder.trim());
        let dev_dir = d.device_dir.clone();
        let skip_dupes = d.skip_dupes;
        let cfg = config_file(SYNC_FOLDER_FILE);
        if let Some(parent) = cfg.parent() {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::fs::write(&cfg, d.pc_folder.trim());
        }
        let placeholders = self.placeholders;
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = src
                .open(false)
                .and_then(|(fs, probe)| {
                    let info = neuros_core::inspect(fs.as_ref(), probe);
                    let known: Vec<_> = info
                        .existing
                        .as_ref()
                        .map(|db| db.tracks().iter().map(|t| t.to_track()).collect())
                        .unwrap_or_default();
                    neuros_core::sync::plan_sync(
                        fs.as_ref(),
                        &known,
                        &pc,
                        &dev_dir,
                        placeholders,
                        skip_dupes,
                        &c2,
                        |d, t| {
                            let _ = tx.send(Msg::Progress(d, t));
                            ctx.request_repaint();
                        },
                    )
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::SyncPlanned(res));
            ctx.request_repaint();
        });
        self.progress = (0, 0);
        self.job = Some(Job { rx, cancel, label: "Comparing" });
    }

    fn start_sync(&mut self, ctx: &egui::Context, plan: SyncPlan, delete_extra: bool) {
        let Some(src) = self.source() else { return };
        let opts = self.write_options();
        let fallback = ScanOptions {
            music_dir: String::new(),
            include_recordings: self.include_recordings,
            placeholders: self.placeholders,
        };
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = (|| {
                let (fs, probe) = src.open(true)?;
                let info = neuros_core::inspect(fs.as_ref(), probe);
                let rep = neuros_core::sync::apply_sync(
                    fs.as_ref(),
                    &info,
                    &plan,
                    delete_extra,
                    &opts,
                    &fallback,
                    |d, t| {
                        let _ = tx.send(Msg::Progress((d / 1024) as usize, (t / 1024) as usize));
                        ctx.request_repaint();
                    },
                )?;
                fs.finish()?;
                Ok::<_, neuros_core::Error>(rep)
            })()
            .map_err(|e| e.to_string());
            let _ = tx.send(Msg::SyncDone(res));
            ctx.request_repaint();
        });
        self.progress = (0, 0);
        self.items.clear();
        self.scanned = None;
        self.job = Some(Job { rx, cancel: Arc::new(AtomicBool::new(false)), label: "Copying" });
        self.say("Syncing…");
    }

    fn sync_dialog(&mut self, ctx: &egui::Context) {
        if self.sync.is_none() {
            return;
        }
        let busy = self.busy();
        let progress = self.progress;
        let top_dirs = self.top_dirs.clone();
        let mode_raw = self.mode == Mode::Raw;
        let mut compare = false;
        let mut go: Option<(SyncPlan, bool)> = None;
        let mut close = false;
        let d = self.sync.as_mut().unwrap();
        let resp = egui::Modal::new(egui::Id::unique("sync")).show(ctx, |ui| {
            ui.set_width(860.0);
            ui.heading("Sync from PC to Neuros");
            ui.label("Makes a folder on the Neuros match a music folder on this computer: new and changed files are copied and the database is updated.");
            ui.add_space(6.0);
            ui.add_enabled_ui(!busy, |ui| {
                egui::Grid::new("syncgrid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                    ui.label("PC folder:");
                    ui.horizontal(|ui| {
                        if ui
                            .add(egui::TextEdit::singleline(&mut d.pc_folder).desired_width(460.0).hint_text("e.g. /home/you/Music"))
                            .changed()
                        {
                            d.plan = None;
                        }
                        if ui.button("Browse…").clicked()
                            && let Some(p) = rfd::FileDialog::new().set_title("Music folder on this computer").pick_folder()
                        {
                            d.pc_folder = p.display().to_string();
                            d.plan = None;
                        }
                    });
                    ui.end_row();
                    ui.label("Neuros folder:");
                    ui.horizontal(|ui| {
                        if ui.add(egui::TextEdit::singleline(&mut d.device_dir).desired_width(200.0)).changed() {
                            d.plan = None;
                        }
                        egui::ComboBox::from_id_salt("syncdir").selected_text("…").show_ui(ui, |ui| {
                            for t in &top_dirs {
                                if ui.selectable_label(false, t).clicked() {
                                    d.device_dir = t.clone();
                                    d.plan = None;
                                }
                            }
                        });
                        ui.label("(PC subfolders are mirrored inside it)");
                    });
                    ui.end_row();
                    ui.label("");
                    if ui
                        .checkbox(&mut d.skip_dupes, "Skip PC files that are second copies of a song already on the Neuros")
                        .on_hover_text("Same artist, title and album, e.g. two rips of one album on the PC. Keeps duplicates you removed from coming back.")
                        .changed()
                    {
                        d.plan = None;
                    }
                    ui.end_row();
                });
            });
            ui.add_space(6.0);
            if busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    let (a, b) = progress;
                    if b > 0 {
                        ui.add(
                            egui::ProgressBar::new(a as f32 / b as f32).desired_width(300.0).text(format!("Comparing {a}/{b}")),
                        );
                    } else {
                        ui.label("Comparing…");
                    }
                });
            }

            if let Some(plan) = &d.plan {
                let (n, nb) = plan.count(SyncAction::Copy);
                let (c, cb) = plan.count(SyncAction::Update);
                let (u, _) = plan.count(SyncAction::Unchanged);
                ui.separator();
                egui::Grid::new("syncsum").num_columns(2).show(ui, |ui| {
                    ui.strong("New:");
                    ui.label(format!("{n} files ({})", fmt_mb(nb)));
                    ui.end_row();
                    ui.strong("Changed:");
                    ui.label(format!("{c} files ({}) — copied again", fmt_mb(cb)));
                    ui.end_row();
                    ui.strong("Unchanged:");
                    ui.label(format!("{u} files — not copied, info refreshed from the PC"));
                    ui.end_row();
                    let (skipped, _) = plan.count(SyncAction::Skip);
                    if skipped > 0 {
                        ui.strong("Duplicates:");
                        ui.label(format!("{skipped} PC files skipped — the same song from the same album is already there"));
                        ui.end_row();
                    }
                    let (renamed, retagged) = (plan.matched(Match::Content), plan.matched(Match::Tags));
                    if renamed + retagged > 0 {
                        ui.label("");
                        ui.label(format!(
                            "{renamed} found on the Neuros under another name{} — they keep their Neuros names",
                            if retagged > 0 { format!(", {retagged} matched by artist/album/title") } else { String::new() }
                        ));
                        ui.end_row();
                    }
                    ui.strong("Only on the Neuros:");
                    ui.horizontal(|ui| {
                        ui.label(format!("{} files ({})", plan.extra.len(), fmt_mb(plan.extra_bytes())));
                        if !plan.extra.is_empty() {
                            ui.checkbox(&mut d.delete_extra, "remove them from the Neuros");
                        }
                    });
                    ui.end_row();
                });
                if d.delete_extra && !plan.extra.is_empty() {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!("{} files will be deleted from the Neuros.", plan.extra.len()),
                    );
                }
                ui.checkbox(&mut d.show_unchanged, "Show unchanged files");

                // (action, title, artist, album, destination)
                let mut rows: Vec<[String; 5]> = plan
                    .entries
                    .iter()
                    .filter(|e| d.show_unchanged || e.action != SyncAction::Unchanged)
                    .map(|e| {
                        let a = match (e.action, e.matched) {
                            (SyncAction::Skip, _) => "duplicate",
                            (SyncAction::Copy, _) => "new",
                            (_, Match::Tags) => "re-tagged",
                            (SyncAction::Update, _) => "changed",
                            (_, Match::Content) => "other name",
                            _ => "same",
                        };
                        [
                            a.to_string(),
                            e.tags.title.clone(),
                            e.tags.artist.clone().unwrap_or_default(),
                            e.tags.album.clone().unwrap_or_default(),
                            match &e.dup_of {
                                Some(p) => format!("already there: {p}"),
                                None => e.dest.clone(),
                            },
                        ]
                    })
                    .collect();
                rows.extend(plan.extra.iter().map(|(p, _)| {
                    let a = if d.delete_extra { "remove" } else { "keep" };
                    [a.to_string(), String::new(), String::new(), String::new(), p.clone()]
                }));
                ui.add_space(4.0);
                TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .max_scroll_height(300.0)
                    .column(Column::exact(76.0))
                    .column(Column::initial(200.0).clip(true))
                    .column(Column::initial(140.0).clip(true))
                    .column(Column::initial(140.0).clip(true))
                    .column(Column::remainder().at_least(150.0).clip(true))
                    .header(20.0, |mut h| {
                        for c in ["", "Title", "Artist", "Album", "On the Neuros"] {
                            h.col(|ui| {
                                ui.strong(c);
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(20.0, rows.len(), |mut row| {
                            let r = &rows[row.index()];
                            for v in r {
                                row.col(|ui| {
                                    ui.label(v);
                                });
                            }
                        });
                    });
                if mode_raw {
                    ui.label("The disk is written directly. Don't disconnect it until this finishes.");
                }
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let can_compare = !busy && !d.pc_folder.trim().is_empty();
                let label = if d.plan.is_some() { "Compare again" } else { "Compare" };
                if ui.add_enabled(can_compare, egui::Button::new(label)).clicked() {
                    compare = true;
                }
                if let Some(plan) = &d.plan {
                    let work = plan.count(SyncAction::Copy).0
                        + plan.count(SyncAction::Update).0
                        + if d.delete_extra { plan.extra.len() } else { 0 };
                    let label = if work == 0 { "Update database" } else { "Sync now" };
                    if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                        go = Some((plan.clone(), d.delete_extra));
                    }
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if resp.should_close() && !busy {
            close = true;
        }
        if compare {
            self.start_sync_plan(ctx);
        } else if let Some((plan, del)) = go {
            self.sync = None;
            self.start_sync(ctx, plan, del);
        } else if close {
            if let Some(job) = &self.job {
                job.cancel.store(true, Ordering::Relaxed);
            }
            self.sync = None;
        }
    }
}

impl App {
    fn start_find_dupes(&mut self, ctx: &egui::Context) {
        let (Some(src), Some(d)) = (self.source(), &self.dupes) else { return };
        let other = d.other_albums;
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = src
                .open(false)
                .and_then(|(fs, probe)| {
                    let info = neuros_core::inspect(fs.as_ref(), probe);
                    neuros_core::dedupe::find_duplicates(fs.as_ref(), &info, other, &c2, |d, t| {
                        let _ = tx.send(Msg::Progress(d, t));
                        ctx.request_repaint();
                    })
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::DupesFound(res));
            ctx.request_repaint();
        });
        self.progress = (0, 0);
        self.job = Some(Job { rx, cancel, label: "Comparing" });
    }

    fn start_fix_text(&mut self, ctx: &egui::Context) {
        let Some(src) = self.source() else { return };
        let opts = self.write_options();
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = (|| {
                let (fs, probe) = src.open(true)?;
                let info = neuros_core::inspect(fs.as_ref(), probe);
                let (changes, rep) = neuros_core::textfix::fix_database(fs.as_ref(), &info, &opts)?;
                fs.finish()?;
                Ok::<_, neuros_core::Error>((changes.len(), rep))
            })()
            .map_err(|e| e.to_string());
            let _ = tx.send(Msg::TextFixed(res));
            ctx.request_repaint();
        });
        self.progress = (0, 0);
        self.items.clear();
        self.scanned = None;
        self.job = Some(Job { rx, cancel: Arc::new(AtomicBool::new(false)), label: "Writing" });
        self.say("Fixing text…");
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(sel) = &self.confirm_delete else { return };
        let tracks: Vec<&Track> = sel.iter().filter_map(|&i| self.device_tracks.get(i)).collect();
        let kb: u64 = tracks.iter().map(|t| t.size_kb as u64).sum();
        let mut go = false;
        let mut close = false;
        let resp = egui::Modal::new(egui::Id::unique("delete-tracks")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading(if tracks.len() == 1 {
                "Delete this track?".to_string()
            } else {
                format!("Delete {} tracks?", tracks.len())
            });
            ui.add_space(4.0);
            for t in tracks.iter().take(8) {
                let who = t.artist.as_deref().map(|a| format!(" — {a}")).unwrap_or_default();
                ui.label(format!("• {}{who}", t.title));
            }
            if tracks.len() > 8 {
                ui.weak(format!("… and {} more", tracks.len() - 8));
            }
            ui.add_space(6.0);
            ui.label(format!(
                "The files ({:.1} MB) are deleted from the Neuros and removed from the database. This can't be undone.",
                kb as f64 / 1024.0
            ));
            if self.mode == Mode::Raw {
                ui.label("The disk is written directly. Don't disconnect it until this finishes.");
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Delete").clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if resp.should_close() {
            close = true;
        }
        if go {
            let paths: Vec<String> =
                tracks.iter().map(|t| t.device_path.get(3..).unwrap_or(&t.device_path).to_string()).collect();
            self.confirm_delete = None;
            self.selected.clear();
            let ctx = ctx.clone();
            self.start_delete(&ctx, paths);
        } else if close {
            self.confirm_delete = None;
        }
    }

    fn text_fix_dialog(&mut self, ctx: &egui::Context) {
        let Some(changes) = &self.text_fixes else { return };
        let mut go = false;
        let mut close = false;
        let mode_raw = self.mode == Mode::Raw;
        let resp = egui::Modal::new(egui::Id::unique("textfix")).show(ctx, |ui| {
            ui.set_width(860.0);
            ui.heading("Fix \"?\" in text");
            let fields: usize = changes.iter().map(|c| c.tracks).sum();
            ui.label(format!(
                "{} values ({fields} track fields) have a \"?\" where an old tagger lost an apostrophe or dash. \
                 Only clear cases are fixed; real question marks and anything unclear stay as they are. \
                 The files aren't changed — scans, adds and syncs fix these the same way from now on.",
                changes.len()
            ));
            ui.add_space(4.0);
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .max_scroll_height(380.0)
                .column(Column::exact(56.0))
                .column(Column::exact(44.0))
                .column(Column::initial(330.0).clip(true))
                .column(Column::remainder().at_least(200.0).clip(true))
                .header(20.0, |mut h| {
                    for c in ["Field", "Tracks", "Now", "Becomes"] {
                        h.col(|ui| {
                            ui.strong(c);
                        });
                    }
                })
                .body(|body| {
                    body.rows(20.0, changes.len(), |mut row| {
                        let c = &changes[row.index()];
                        row.col(|ui| {
                            ui.label(c.field.name());
                        });
                        row.col(|ui| {
                            ui.label(c.tracks.to_string());
                        });
                        row.col(|ui| {
                            ui.label(&c.old);
                        });
                        row.col(|ui| {
                            ui.label(&c.new);
                        });
                    });
                });
            ui.add_space(6.0);
            if mode_raw {
                ui.label("The disk is written directly. Don't disconnect it until this finishes.");
            }
            ui.horizontal(|ui| {
                if ui.button(format!("Fix {} values and update database", changes.len())).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if resp.should_close() {
            close = true;
        }
        if go {
            self.text_fixes = None;
            self.start_fix_text(ctx);
        } else if close {
            self.text_fixes = None;
        }
    }

    fn start_delete(&mut self, ctx: &egui::Context, paths: Vec<String>) {
        let Some(src) = self.source() else { return };
        let opts = self.write_options();
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = (|| {
                let (fs, probe) = src.open(true)?;
                let info = neuros_core::inspect(fs.as_ref(), probe);
                let rep = neuros_core::dedupe::delete_tracks(fs.as_ref(), &info, &paths, &opts)?;
                fs.finish()?;
                Ok::<_, neuros_core::Error>(rep)
            })()
            .map_err(|e| e.to_string());
            let _ = tx.send(Msg::DupesDeleted(res));
            ctx.request_repaint();
        });
        self.progress = (0, 0);
        self.items.clear();
        self.scanned = None;
        self.job = Some(Job { rx, cancel: Arc::new(AtomicBool::new(false)), label: "Deleting" });
        self.say("Deleting…");
    }

    fn dupes_dialog(&mut self, ctx: &egui::Context) {
        if self.dupes.is_none() {
            return;
        }
        let busy = self.busy();
        let mode_raw = self.mode == Mode::Raw;
        let mut rerun = false;
        let mut go: Option<Vec<String>> = None;
        let mut close = false;
        let d = self.dupes.as_mut().unwrap();
        let resp = egui::Modal::new(egui::Id::unique("dupes")).show(ctx, |ui| {
            ui.set_width(900.0);
            ui.heading("Duplicates on the Neuros");
            ui.label(
                "Songs that are on the Neuros more than once. Ticked copies are deleted from the Neuros; \
                 identical files and copies from the same album are ticked for you, keeping the copy with \
                 proper tags and the best quality.",
            );
            ui.add_space(4.0);
            ui.add_enabled_ui(!busy && !d.confirm, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .checkbox(
                            &mut d.other_albums,
                            "Also show the same song on other albums (best-ofs, compilations)",
                        )
                        .changed()
                    {
                        rerun = true;
                    }
                    ui.separator();
                    if ui.button("Tick suggested").clicked() {
                        d.tick_suggested();
                    }
                    if ui.button("Untick all").clicked() {
                        for g in &mut d.del {
                            g.fill(false);
                        }
                    }
                });
            });
            ui.add_space(4.0);
            if busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Working…");
                });
            } else if d.groups.is_empty() {
                ui.label("No duplicates found.");
            } else {
                // Flatten: a header row per group, then its files.
                let rows: Vec<(usize, Option<usize>)> = d
                    .groups
                    .iter()
                    .enumerate()
                    .flat_map(|(g, grp)| {
                        std::iter::once((g, None)).chain((0..grp.files.len()).map(move |i| (g, Some(i))))
                    })
                    .collect();
                let row_h = 20.0;
                egui::ScrollArea::vertical().max_height(420.0).auto_shrink([false, true]).show_rows(
                    ui,
                    row_h,
                    rows.len(),
                    |ui, range| {
                        for &(g, i) in &rows[range] {
                            let grp = &d.groups[g];
                            ui.horizontal(|ui| {
                                ui.set_height(row_h);
                                match i {
                                    None => {
                                        ui.strong(&grp.name);
                                        if d.del[g].iter().all(|&x| x) {
                                            ui.colored_label(ui.visuals().error_fg_color, "— every copy is ticked!");
                                        }
                                    }
                                    Some(i) => {
                                        let f = &grp.files[i];
                                        ui.add_space(16.0);
                                        ui.add_enabled_ui(!d.confirm, |ui| {
                                            ui.checkbox(&mut d.del[g][i], "").on_hover_text("Delete this copy");
                                        });
                                        let rel = if f.relation == Relation::Keeper {
                                            "suggested keep"
                                        } else {
                                            f.relation.label()
                                        };
                                        ui.add_sized([96.0, row_h], egui::Label::new(rel));
                                        let t = f.track.duration_secs;
                                        ui.add_sized(
                                            [44.0, row_h],
                                            egui::Label::new(format!("{}:{:02}", t / 60, t % 60)),
                                        );
                                        ui.add_sized([70.0, row_h], egui::Label::new(format!("{} kbps", f.kbps)));
                                        ui.add_sized([64.0, row_h], egui::Label::new(fmt_mb(f.size)));
                                        let text = if d.del[g][i] {
                                            egui::RichText::new(&f.path).strikethrough()
                                        } else {
                                            egui::RichText::new(&f.path)
                                        };
                                        ui.add(egui::Label::new(text).truncate()).on_hover_text(format!(
                                            "{}\nartist: {}\nalbum: {}",
                                            f.track.title,
                                            f.track.artist.as_deref().unwrap_or("-"),
                                            f.track.album.as_deref().unwrap_or("-")
                                        ));
                                    }
                                }
                            });
                        }
                    },
                );
            }

            ui.add_space(6.0);
            let (paths, bytes) = d.ticked();
            let all = d.all_ticked();
            if all > 0 {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("{all} song(s) would have no copy left. Untick at least one copy of each."),
                );
            }
            if mode_raw {
                ui.label("The disk is written directly. Don't disconnect it until this finishes.");
            }
            ui.horizontal(|ui| {
                if d.confirm {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!(
                            "Really delete {} files ({}) from the Neuros? This can't be undone.",
                            paths.len(),
                            fmt_mb(bytes)
                        ),
                    );
                    if ui.button("Delete").clicked() {
                        go = Some(paths.clone());
                    }
                    if ui.button("Back").clicked() {
                        d.confirm = false;
                    }
                } else {
                    let label = format!("Delete {} files ({}) and update database", paths.len(), fmt_mb(bytes));
                    if ui.add_enabled(!busy && !paths.is_empty() && all == 0, egui::Button::new(label)).clicked() {
                        d.confirm = true;
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                }
            });
        });
        if resp.should_close() && !busy {
            close = true;
        }
        if rerun {
            self.start_find_dupes(ctx);
        } else if let Some(paths) = go {
            self.dupes = None;
            self.start_delete(ctx, paths);
        } else if close {
            self.dupes = None;
        }
    }
}

impl AddDialog {
    fn layout(&self) -> AddLayout {
        match self.kind {
            AddKind::ByTags => AddLayout::ByTags,
            AddKind::KeepFolders => AddLayout::KeepFolders(self.folder.clone()),
            AddKind::OneFolder => AddLayout::Folder(self.folder.clone()),
        }
    }

    fn update_dests(&mut self) {
        let layout = self.layout();
        for p in &mut self.plan {
            p.dest = add_destination(&p.tags, &p.rel, &layout, &self.music_root);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        let ctx = ui.ctx().clone();

        egui::Panel::top("device").show(ui, |ui| {
            ui.add_space(6.0);
            self.device_panel(ui);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::bottom("log").resizable(true).default_size(100.0).show(ui, |ui| {
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                for l in &self.log {
                    ui.monospace(l);
                }
            });
        });
        if self.view == View::Device && !self.selected.is_empty() {
            egui::Panel::right("edit").resizable(true).default_size(300.0).show(ui, |ui| self.edit_panel(ui));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view, View::Device, format!("On Neuros ({})", self.device_tracks.len()));
                ui.add_enabled_ui(!self.items.is_empty(), |ui| {
                    ui.selectable_value(&mut self.view, View::Scan, format!("Scan results ({})", self.items.len()));
                });
                if self.edits_dirty() {
                    ui.separator();
                    ui.colored_label(ui.visuals().warn_fg_color, "Unsaved edits");
                    if ui.add_enabled(!self.busy(), egui::Button::new("💾 Save edits to Neuros")).clicked() {
                        let ctx = ui.ctx().clone();
                        self.start_save_edits(&ctx);
                    }
                    if ui.button("Discard").clicked() {
                        if let Some(info) = &self.info {
                            self.edits = info.overrides.clone();
                        }
                        self.edit = EditBuf::default();
                    }
                }
            });
            ui.separator();
            match self.view {
                View::Scan if !self.items.is_empty() => self.table(ui),
                View::Device if !self.device_tracks.is_empty() => self.device_table(ui),
                _ => {
                    let label = self.job.as_ref().map(|j| j.label);
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() / 3.0);
                        match label {
                            Some(l) => {
                                ui.add(egui::Spinner::new().size(36.0));
                                ui.add_space(8.0);
                                ui.heading(format!("{l}…"));
                                if l == "Reading the database" {
                                    ui.weak("Over the Neuros's own USB (1.1) this can take a minute.");
                                }
                            }
                            None if self.info.is_none() => {
                                ui.heading("No Neuros open");
                                ui.weak("Plug it in and press ⟳ Detect, use Browse…, or choose \"Hard disk on USB-IDE adapter\".");
                            }
                            None => {
                                ui.heading("No database on the Neuros yet");
                                ui.weak("Press 🔍 Scan device, then 💾 Write database, or use Add / Sync from PC.");
                            }
                        }
                    });
                }
            }
        });
        self.options_window(&ctx);
        self.confirm_dialog(&ctx);
        self.add_dialog(&ctx);
        self.sync_dialog(&ctx);
        self.dupes_dialog(&ctx);
        self.text_fix_dialog(&ctx);
        self.delete_dialog(&ctx);

        // Files or folders dropped onto the window → Add music.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect()
        });
        if !dropped.is_empty() {
            if self.busy() || self.info.is_none() {
                self.say("Open a device first, then drop files to add them.");
            } else {
                self.open_add(dropped);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(title: &str, artist: Option<&str>, album: &str, n: u32, secs: u32, path: &str) -> Track {
        Track {
            title: title.into(),
            artist: artist.map(Into::into),
            album: Some(album.into()),
            genre: None,
            recording: None,
            disc: None,
            track_no: Some(n),
            duration_secs: secs,
            size_kb: 1,
            device_path: path.into(),
        }
    }

    #[test]
    fn natural_order() {
        use std::cmp::Ordering::*;
        assert_eq!(natural_cmp("2._x", "10._x"), Less);
        assert_eq!(natural_cmp("abc", "ABD"), Less);
        assert_eq!(natural_cmp("track 02", "track 2"), Equal);
        assert_eq!(natural_cmp("a", "a1"), Less);
    }

    #[test]
    fn sorting() {
        let rows = vec![
            (0, false, t("Zed", Some("bravo"), "Second", 2, 200, "C:/m/10.mp3")),
            (1, true, t("alpha", None, "X", 1, 100, "C:/m/2.mp3")),
            (2, false, t("Mid", Some("Able"), "First", 1, 300, "C:/m/1.mp3")),
            (3, false, t("Dusk", Some("Bravo"), "Second", 1, 50, "C:/m/3.mp3")),
        ];
        assert_eq!(sorted_rows(rows.clone(), (SortCol::Title, false)), [1, 3, 2, 0]);
        assert_eq!(sorted_rows(rows.clone(), (SortCol::Title, true)), [0, 2, 3, 1]);
        // Empty artist last; same artist keeps album track order.
        assert_eq!(sorted_rows(rows.clone(), (SortCol::Artist, false)), [2, 3, 0, 1]);
        assert_eq!(sorted_rows(rows.clone(), (SortCol::Time, false)), [3, 1, 0, 2]);
        assert_eq!(sorted_rows(rows.clone(), (SortCol::Path, false)), [2, 1, 3, 0]);
        assert_eq!(sorted_rows(rows, (SortCol::Mark, false))[0], 1);
    }
}
