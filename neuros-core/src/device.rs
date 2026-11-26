//! Device access (mounted folder or raw disk), drive-letter choice, backup
//! and safe writing.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::rawdisk::{self, Probe, SwapMode};
use crate::reader::{Database, load_database};
use crate::vfs::{self, StdVfs, Vfs};
use crate::{BuildOptions, Error, Result, ScanItem, build_audio, templates, verify};

pub const DB_DIR: &str = "WOID_DB";

/// Drive letter the firmware uses in paths: C: when the hard-disk backpack
/// is in use, D: for internal flash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drive {
    C,
    D,
}

impl Drive {
    pub fn prefix(self) -> &'static str {
        match self {
            Drive::C => "C:",
            Drive::D => "D:",
        }
    }
}

/// Where the Neuros filesystem is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Mounted by the OS (Neuros plugged in over its own USB port).
    Mounted(PathBuf),
    /// Raw disk or disk image, read with our own FAT driver.  The Neuros
    /// hard disk on a USB-IDE adapter appears byte-swapped.
    Raw { path: PathBuf, swap: SwapMode },
}

impl Source {
    pub fn describe(&self) -> String {
        match self {
            Source::Mounted(p) => p.display().to_string(),
            Source::Raw { path, .. } => path.display().to_string(),
        }
    }

    /// Open for reading or writing.  Raw disks that the OS has mounted are
    /// refused for writing.
    pub fn open(&self, writable: bool) -> Result<(Box<dyn Vfs>, Option<Probe>)> {
        match self {
            Source::Mounted(p) => {
                if !p.is_dir() {
                    return Err(Error::Other(format!("{} is not a folder", p.display())));
                }
                Ok((Box::new(StdVfs::new(p.clone())), None))
            }
            Source::Raw { path, swap } => {
                if writable && rawdisk::is_mounted(path) {
                    return Err(Error::Other(format!("{} is mounted by the OS; unmount it first", path.display())));
                }
                let probe = rawdisk::probe(path, *swap)?;
                let fs = rawdisk::open_fat(&probe, writable)?;
                Ok((Box::new(fs), Some(probe)))
            }
        }
    }
}

/// Filesystems smaller than this are assumed to be the flash unit (same
/// heuristic as positron).
const FLASH_CUTOFF: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug)]
pub struct DeviceInfo {
    pub location: String,
    pub probe: Option<Probe>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub total_bytes: Option<u64>,
    /// Device-relative path of WOID_DB, if present.
    pub db_dir: Option<String>,
    pub drive: Drive,
    pub drive_reason: String,
    pub existing: Option<Database>,
    pub existing_error: Option<String>,
    /// Suggested setting for the Artist/Album menu, and why.
    pub artist_albums: bool,
    pub artist_albums_reason: String,
    /// Field edits stored on the device (applied on every rebuild).
    pub overrides: crate::overrides::Overrides,
}

/// Firmware that knows the Artist/Album sub-database (per NeurosDBM).
pub const ARTIST_ALBUM_FIRMWARE: (u32, u32) = (2, 14);

/// Parse "2.28", "2.28-beta", "1-45" into (major, minor).
pub fn parse_firmware(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    let (a, rest) = s.split_once(['.', '-'])?;
    let b: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    Some((a.trim().parse().ok()?, b.parse().ok()?))
}

impl DeviceInfo {
    /// Does the existing database have the Artist/Album sub-database?
    pub fn existing_has_artist_albums(&self) -> bool {
        self.existing.as_ref().is_some_and(|db| {
            db.children.iter().any(|c| c.subs.iter().any(|s| s.file_stem.eq_ignore_ascii_case("artistalbum")))
        })
    }

    pub fn looks_like_neuros(&self) -> bool {
        self.serial.is_some() || self.db_dir.is_some()
    }

    /// Recording type of existing tracks, keyed by lower-cased device path.
    pub fn existing_recordings(&self) -> HashMap<String, String> {
        let mut m = HashMap::new();
        if let Some(db) = &self.existing {
            for t in db.tracks() {
                if let Some(r) = t.keys.get(4).and_then(|v| v.first()) {
                    m.insert(t.path.to_lowercase(), r.clone());
                }
            }
        }
        m
    }
}

fn read_first_line(vfs: &dyn Vfs, name: &str) -> Option<String> {
    let p = vfs::find_file_ci(vfs, "", name)?;
    let s = String::from_utf8_lossy(&vfs.read_file(&p).ok()?).into_owned();
    let l = s.lines().next()?.trim().to_string();
    (!l.is_empty()).then_some(l)
}

/// Gather what we know about an opened device.
pub fn inspect(vfs: &dyn Vfs, probe: Option<Probe>) -> DeviceInfo {
    let serial = read_first_line(vfs, "sn.txt");
    let firmware = read_first_line(vfs, "version.txt");
    let total_bytes = vfs.total_space();
    let db_dir = vfs::find_dir_ci(vfs, "", DB_DIR);
    let (existing, existing_error) = match db_dir.as_ref().and_then(|d| vfs::find_dir_ci(vfs, d, "audio")) {
        Some(audio) => match load_database(vfs, &audio, "audio") {
            Ok(db) => (Some(db), None),
            Err(e) => (None, Some(e.to_string())),
        },
        None => (None, None),
    };

    // 1. Trust the drive letter already used by the existing database.
    let mut counts = (0usize, 0usize);
    if let Some(db) = &existing {
        for t in db.tracks() {
            let p = t.path.to_ascii_uppercase();
            if p.starts_with("C:") {
                counts.0 += 1;
            } else if p.starts_with("D:") {
                counts.1 += 1;
            }
        }
    }
    let (drive, drive_reason) = if counts.0 + counts.1 > 0 {
        let d = if counts.0 >= counts.1 { Drive::C } else { Drive::D };
        (d, format!("from existing database ({} tracks use {})", counts.0.max(counts.1), d.prefix()))
    } else if probe.as_ref().is_some_and(|p| p.swapped) {
        // 2. A byte-swapped disk can only be the hard disk.
        (Drive::C, "byte-swapped hard disk".into())
    } else {
        // 3. Fall back to filesystem size.
        match total_bytes {
            Some(sz) if sz < FLASH_CUTOFF => (Drive::D, format!("{} MB filesystem → flash", sz >> 20)),
            Some(sz) => (Drive::C, format!("{} GB filesystem → hard disk", sz >> 30)),
            None => (Drive::C, "unknown size; assuming hard disk".into()),
        }
    };

    let has_aa = existing.as_ref().is_some_and(|db| {
        db.children.iter().any(|c| c.subs.iter().any(|s| s.file_stem.eq_ignore_ascii_case("artistalbum")))
    });
    let fw = firmware.as_deref().and_then(parse_firmware);
    let (artist_albums, artist_albums_reason) = match fw {
        _ if has_aa => (true, "the current database has it".to_string()),
        Some(v) if v >= ARTIST_ALBUM_FIRMWARE => (true, format!("firmware {}.{} supports it", v.0, v.1)),
        Some(v) => (false, format!("firmware {}.{} is older than 2.14", v.0, v.1)),
        None => (false, "firmware version unknown".to_string()),
    };

    let db_dir_for_edits = db_dir.clone();
    DeviceInfo {
        location: vfs.describe(),
        probe,
        serial,
        firmware,
        total_bytes,
        db_dir,
        drive,
        drive_reason,
        existing,
        existing_error,
        artist_albums,
        artist_albums_reason,
        overrides: crate::overrides::Overrides::load(vfs, db_dir_for_edits.as_deref()),
    }
}

/// Per-user settings file for neuros-db (e.g. the last sync folder).
pub fn config_file(name: &str) -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("neuros-db").join(name)
}

/// Default host directory for database backups.
pub fn default_backup_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("neuros-db").join("backups")
}

/// `YYYYMMDD-HHMMSS` in UTC, without pulling in a date crate.
pub fn timestamp() -> String {
    let secs =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem / 60 % 60, rem % 60)
}

#[derive(Debug, Clone)]
pub struct WriteOptions {
    pub build: BuildOptions,
    /// Rename files with non-ASCII names to their ASCII-safe names.  When
    /// false, such files are left out of the database.
    pub rename_files: bool,
    /// Host folder to copy the current WOID_DB to first.  None = no backup.
    pub backup_dir: Option<PathBuf>,
    /// Drive letter override (None = use the one from `inspect`).
    pub drive: Option<Drive>,
    /// Build and verify in memory only; touch nothing on the device.
    pub dry_run: bool,
}

#[derive(Debug, Default)]
pub struct WriteReport {
    pub tracks_written: usize,
    pub renamed: Vec<(String, String)>,
    pub skipped: Vec<(String, String)>,
    pub backup: Option<PathBuf>,
    pub created_dbs: Vec<String>,
    /// Artist entries in the Artist → Album menu (if written).
    pub artist_album_entries: Option<usize>,
    /// Files copied onto the device by `add_files` (device-relative).
    pub added: Vec<String>,
    /// Files `add_files` found already on the device (same name and size).
    pub already_present: Vec<String>,
    /// Files removed by a sync.
    pub deleted: Vec<String>,
    /// Tracks with stored field edits after this write.
    pub edited_tracks: usize,
}

/// Rename `from` to `to` (device-relative), creating directories and
/// avoiding collisions.  Returns the final path.
fn rename_into(vfs: &dyn Vfs, from: &str, to: &str) -> Result<String> {
    vfs::create_dir_all(vfs, vfs::parent(to))?;
    let mut target = to.to_string();
    let mut n = 2;
    while vfs::exists(vfs, &target) {
        let name = vfs::file_name(to);
        let (stem, ext) = name.rsplit_once('.').map(|(s, e)| (s, format!(".{e}"))).unwrap_or((name, String::new()));
        target = vfs::join(vfs::parent(to), &format!("{stem} ({n}){ext}"));
        n += 1;
    }
    vfs.rename(from, &target)?;
    // Remove directories left empty by the move (best effort).
    remove_empty_parents(vfs, vfs::parent(from).to_string());
    Ok(target)
}

/// Build the database from scanned items and write it to the device.
///
/// Order: build + verify in memory → back up WOID_DB → rename files →
/// write `audio.new/` → swap it in for `audio/`.  Call `vfs.finish()`
/// afterwards to flush raw disks.
pub fn write_database(
    vfs: &dyn Vfs,
    info: &DeviceInfo,
    items: &[ScanItem],
    opts: &WriteOptions,
) -> Result<WriteReport> {
    let mut report = WriteReport::default();
    let drive = opts.drive.unwrap_or(info.drive);

    let mut tracks = Vec::with_capacity(items.len());
    let mut renames = Vec::new();
    for it in items {
        let mut t = it.track.clone();
        // Re-derive the path in case the drive letter was changed after scanning.
        t.device_path = crate::scan::device_path(drive, it.rename_to.as_deref().unwrap_or(&it.rel_path));
        match (&it.rename_to, opts.rename_files) {
            (Some(to), true) => {
                renames.push((it.rel_path.clone(), to.clone(), tracks.len()));
                tracks.push(t);
            }
            (Some(_), false) => report.skipped.push((it.rel_path.clone(), "non-ASCII file name".into())),
            (None, _) => tracks.push(t),
        }
    }
    if opts.dry_run {
        report.renamed = renames.iter().map(|(f, t, _)| (f.clone(), t.clone())).collect();
    }
    commit(vfs, info, tracks, opts, &info.overrides, report, |tracks, report| {
        for (from, to, ti) in renames {
            let fin = rename_into(vfs, &from, &to)?;
            if fin != to {
                tracks[ti].device_path = crate::scan::device_path(drive, &fin);
            }
            report.renamed.push((from, fin));
        }
        Ok(())
    })
}

/// Build + verify `tracks`, then (unless dry-run) back up, run `change`
/// (which may alter files and tracks), and write the database.
pub(crate) fn commit(
    vfs: &dyn Vfs,
    info: &DeviceInfo,
    mut tracks: Vec<crate::Track>,
    opts: &WriteOptions,
    edits: &crate::overrides::Overrides,
    mut report: WriteReport,
    change: impl FnOnce(&mut Vec<crate::Track>, &mut WriteReport) -> Result<()>,
) -> Result<WriteReport> {
    for t in &mut tracks {
        edits.apply(t);
    }
    // Validate the build before touching the device.
    let check = |tracks: &[crate::Track]| -> Result<Vec<crate::DbFile>> {
        if tracks.len() > 30_000 {
            return Err(Error::Other(format!("{} tracks is more than the database format can hold", tracks.len())));
        }
        let files = build_audio(tracks, &opts.build)?;
        let parsed = crate::reader::load_database_from_files(&files, "audio")?;
        let problems = verify::verify(&parsed);
        if !problems.is_empty() {
            return Err(Error::Corrupt(format!("built database failed verification:\n{}", problems.join("\n"))));
        }
        if parsed.tracks().len() != tracks.len() {
            return Err(Error::Corrupt("built database lost tracks".into()));
        }
        Ok(files)
    };
    let files = check(&tracks)?;
    report.artist_album_entries = files
        .iter()
        .find(|f| f.name == "artistalbum.sai")
        .and_then(|f| crate::reader::parse_sai(&f.bytes, "artistalbum.sai").ok())
        .map(|s| s.len().saturating_sub(1));
    if opts.dry_run {
        report.tracks_written = tracks.len();
        return Ok(report);
    }

    if let (Some(backup_root), Some(existing)) = (&opts.backup_dir, &info.db_dir) {
        let name = format!("{}-{}", info.serial.as_deref().unwrap_or("neuros"), timestamp());
        let name: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
        let dest = backup_root.join(name);
        vfs::copy_to_host(vfs, existing, &dest)?;
        report.backup = Some(dest);
    }

    change(&mut tracks, &mut report)?;
    for t in &mut tracks {
        edits.apply(t);
    }
    let files = check(&tracks)?;

    let db_dir = match &info.db_dir {
        Some(d) => d.clone(),
        None => {
            vfs.create_dir(DB_DIR)?;
            DB_DIR.to_string()
        }
    };

    // Write the new audio DB next to the old one, then swap.
    let new_dir = vfs::join(&db_dir, "audio.new");
    let old_dir = vfs::join(&db_dir, "audio.old");
    for stale in [&new_dir, &old_dir] {
        if let Some(p) = vfs::find_dir_ci(vfs, &db_dir, vfs::file_name(stale)) {
            vfs::remove_dir_all(vfs, &p)?;
        }
    }
    vfs.create_dir(&new_dir)?;
    for f in &files {
        vfs.write_file(&vfs::join(&new_dir, &f.name), &f.bytes)?;
    }
    if let Some(cur) = vfs::find_dir_ci(vfs, &db_dir, "audio") {
        vfs.rename(&cur, &old_dir)?;
    }
    vfs.rename(&new_dir, &vfs::join(&db_dir, "audio"))?;
    if vfs::exists(vfs, &old_dir) {
        vfs::remove_dir_all(vfs, &old_dir)?;
    }

    // Keep the edits file in step with the database.
    let edits = edits.pruned(&tracks);
    let edits_path = vfs::find_file_ci(vfs, &db_dir, crate::overrides::EDITS_FILE)
        .unwrap_or_else(|| vfs::join(&db_dir, crate::overrides::EDITS_FILE));
    if !edits.is_empty() {
        vfs.write_file(&edits_path, edits.to_text().as_bytes())?;
    } else if vfs::exists(vfs, &edits_path) {
        vfs.remove(&edits_path)?;
    }
    report.edited_tracks = edits.len();

    // The firmware expects the other databases to exist; create empty ones.
    for db in templates::ROOT_DBS.iter().filter(|d| **d != "audio") {
        if vfs::find_dir_ci(vfs, &db_dir, db).is_none() {
            let d = vfs::join(&db_dir, db);
            vfs.create_dir(&d)?;
            for (name, bytes) in templates::for_db(db) {
                vfs.write_file(&vfs::join(&d, name), bytes)?;
            }
            report.created_dbs.push(db.to_string());
        }
    }

    report.tracks_written = tracks.len();
    Ok(report)
}

// ---------------------------------------------------------------- adding music

/// Where added files go on the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddLayout {
    /// `<music root>/<Artist>/<Album>/<file>` from the tags.
    ByTags,
    /// All into one device-relative folder.
    Folder(String),
    /// Under a device-relative folder, keeping the folders that were
    /// picked: adding `~/Music/Some Band` gives `<folder>/Some Band/<album>/<file>`.
    KeepFolders(String),
}

/// One file to copy onto the device.
#[derive(Debug, Clone)]
pub struct PlannedAdd {
    pub host: PathBuf,
    /// Path to keep for `KeepFolders`: the picked folder's name plus the
    /// path inside it, or just the file name for files picked one by one.
    pub rel: PathBuf,
    pub size: u64,
    /// Device-relative destination (ASCII-safe).
    pub dest: String,
    pub tags: crate::scan::TagInfo,
}

/// All MP3/Ogg files in `dir` and below (sorted).
pub fn collect_audio_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter(|e| crate::scan::audio_file_type(&e.file_name().to_string_lossy()).is_some())
        .map(|e| e.into_path())
        .collect();
    v.sort();
    v
}

/// Expand a selection of files and folders (folders recursively) into
/// `(audio file, path to keep)`; see `PlannedAdd::rel`.  Duplicates are
/// dropped; non-audio files picked directly are skipped.
pub fn expand_selection(paths: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            let keep_from = p.parent().unwrap_or(p);
            for f in collect_audio_files(p) {
                let rel = f.strip_prefix(keep_from).unwrap_or(&f).to_path_buf();
                if seen.insert(f.clone()) {
                    out.push((f, rel));
                }
            }
        } else if p.file_name().is_some_and(|n| crate::scan::audio_file_type(&n.to_string_lossy()).is_some())
            && seen.insert(p.clone())
        {
            out.push((p.clone(), PathBuf::from(p.file_name().unwrap())));
        }
    }
    out
}

/// Device-relative, ASCII-safe destination for one added file.
pub fn add_destination(
    tags: &crate::scan::TagInfo,
    rel: &std::path::Path,
    layout: &AddLayout,
    music_root: &str,
) -> String {
    use crate::scan::{normalize_rel, sanitize_filename};
    let file = sanitize_filename(&rel.file_name().unwrap_or_default().to_string_lossy());
    let join = |parts: Vec<String>| parts.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/");
    match layout {
        AddLayout::ByTags => {
            let artist = sanitize_filename(tags.artist.as_deref().unwrap_or("Unknown Artist"));
            let album = sanitize_filename(tags.album.as_deref().unwrap_or("Unknown Album"));
            join(vec![normalize_rel(music_root), artist, album, file])
        }
        AddLayout::Folder(f) => vfs::join(&normalize_rel(f), &file),
        AddLayout::KeepFolders(f) => {
            let mut parts = vec![normalize_rel(f)];
            parts.extend(rel.iter().map(|c| sanitize_filename(&c.to_string_lossy())));
            join(parts)
        }
    }
}

/// Read tags of the picked files and folders (folders recursively) and
/// decide where each goes.  `music_root` is the device folder for `ByTags`
/// (e.g. "MUSIC").  Non-audio files are skipped.
pub fn plan_add(selection: &[PathBuf], layout: &AddLayout, music_root: &str, placeholders: bool) -> Vec<PlannedAdd> {
    use crate::scan::read_tags;
    expand_selection(selection)
        .into_iter()
        .filter_map(|(h, rel)| {
            let name = h.file_name()?.to_string_lossy().into_owned();
            let ft = crate::scan::audio_file_type(&name)?;
            let size = std::fs::metadata(&h).map(|m| m.len()).unwrap_or(0);
            let reader = std::fs::File::open(&h)
                .map(|f| Box::new(std::io::BufReader::new(f)) as Box<dyn vfs::ReadSeek>)
                .map_err(Error::from);
            let tags = read_tags(reader, ft, &name, placeholders);
            let dest = add_destination(&tags, &rel, layout, music_root);
            Some(PlannedAdd { host: h, rel, size, dest, tags })
        })
        .collect()
}

/// Copy `planned` files onto the device and update the database.
///
/// The existing database's records are kept as they are (no rescan), so
/// this is quick even on big libraries; files already on the device with
/// the same name and size are not copied again.  If there is no readable
/// database, the whole device is scanned instead using `fallback`.
/// `progress(bytes_done, bytes_total)`.  Call `vfs.finish()` afterwards.
pub fn add_files(
    vfs: &dyn Vfs,
    info: &DeviceInfo,
    planned: &[PlannedAdd],
    opts: &WriteOptions,
    fallback: &crate::ScanOptions,
    mut progress: impl FnMut(u64, u64),
) -> Result<WriteReport> {
    let drive = opts.drive.unwrap_or(info.drive);
    let mut report = WriteReport::default();
    let base = base_tracks(vfs, info, drive, fallback, &mut report)?;
    let total: u64 = planned.iter().map(|p| p.size).sum();

    commit(vfs, info, base, opts, &info.overrides, report, |tracks, report| {
        let mut done = 0u64;
        for p in planned {
            let dir = vfs::create_dir_all(vfs, vfs::parent(&p.dest))?;
            let name = vfs::file_name(&p.dest);
            let mut target = vfs::join(&dir, name);
            let existing = vfs::find_ci(vfs, &dir, name);
            match existing {
                Some((real, e)) if !e.is_dir && e.len == p.size => {
                    target = real; // same file already there
                    report.already_present.push(target.clone());
                }
                _ => {
                    let mut n = 2;
                    while vfs::exists(vfs, &target) {
                        let (stem, ext) =
                            name.rsplit_once('.').map(|(s, e)| (s, format!(".{e}"))).unwrap_or((name, String::new()));
                        target = vfs::join(&dir, &format!("{stem} ({n}){ext}"));
                        n += 1;
                    }
                    let data = std::fs::read(&p.host)?;
                    vfs.write_file(&target, &data)?;
                    report.added.push(target.clone());
                }
            }
            done += p.size;
            progress(done, total);

            let device_path = crate::scan::device_path(drive, &target);
            tracks.retain(|t| !t.device_path.eq_ignore_ascii_case(&device_path));
            tracks.push(track_from(&p.tags, p.size, device_path));
        }
        Ok(())
    })
}

/// A database track from tags read on the host.
pub(crate) fn track_from(t: &crate::scan::TagInfo, size: u64, device_path: String) -> crate::Track {
    crate::Track {
        title: t.title.clone(),
        artist: t.artist.clone(),
        album: t.album.clone(),
        genre: t.genre.clone(),
        recording: None,
        disc: t.disc,
        track_no: t.track_no,
        duration_secs: t.duration_secs,
        size_kb: (size / 1024) as u32,
        device_path,
    }
}

/// Tracks of the current database, or (if there is none) a full scan.
pub(crate) fn base_tracks(
    vfs: &dyn Vfs,
    info: &DeviceInfo,
    drive: Drive,
    fallback: &crate::ScanOptions,
    report: &mut WriteReport,
) -> Result<Vec<crate::Track>> {
    Ok(match &info.existing {
        Some(db) => db.tracks().iter().map(|t| t.to_track()).collect(),
        None => {
            let items =
                crate::scan::scan(vfs, drive, fallback, &info.existing_recordings(), &Default::default(), |_, _| {})?;
            items
                .into_iter()
                .filter_map(|it| match it.rename_to {
                    None => Some(it.track),
                    Some(_) => {
                        report.skipped.push((it.rel_path, "non-ASCII file name".into()));
                        None
                    }
                })
                .collect()
        }
    })
}

/// Remove `dir` and its parents while they are empty (stops at the root).
pub(crate) fn remove_empty_parents(vfs: &dyn Vfs, mut dir: String) {
    while !dir.is_empty() && vfs.read_dir(&dir).map(|e| e.is_empty()).unwrap_or(false) {
        if vfs.remove(&dir).is_err() {
            break;
        }
        dir = vfs::parent(&dir).to_string();
    }
}

/// Save field edits: rebuild the database from its current records with
/// `edits` (the complete set, replacing what is stored) applied.
pub fn save_edits(
    vfs: &dyn Vfs,
    info: &DeviceInfo,
    edits: &crate::overrides::Overrides,
    opts: &WriteOptions,
) -> Result<WriteReport> {
    let db = info.existing.as_ref().ok_or_else(|| Error::Other("there is no database on the device to edit".into()))?;
    let tracks = db.tracks().iter().map(|t| t.to_track()).collect();
    commit(vfs, info, tracks, opts, edits, WriteReport::default(), |_, _| Ok(()))
}
