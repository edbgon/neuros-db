//! Scanning a folder on the device for MP3 / Ogg Vorbis files.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use lofty::file::FileType;
use lofty::prelude::*;
use lofty::probe::Probe;

use crate::device::Drive;
use crate::vfs::{self, Vfs};
use crate::{Error, Result, Track};

pub const EXTENSIONS: [&str; 2] = ["mp3", "ogg"];
/// Device directories that never contain music to index.
const SKIP_DIRS: [&str; 5] = ["woid_db", "woid_sync", "woid_hisi", "system volume information", "$recycle.bin"];
pub const RECORD_DIR: &str = "WOID_RECORD";

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Device-relative folder to scan ("" = whole device).
    pub music_dir: String,
    /// Also index the device's own recordings in WOID_RECORD.
    pub include_recordings: bool,
    /// Use "Unknown Artist" etc. instead of leaving the key empty, so the
    /// track still shows up in the Artists/Albums/Genres menus.
    pub placeholders: bool,
}

#[derive(Debug, Clone)]
pub struct ScanItem {
    pub track: Track,
    /// Device-relative path of the file now.
    pub rel_path: String,
    /// If the file name is not plain ASCII (which locks up the Neuros), the
    /// ASCII-safe device-relative path it should be renamed to.
    pub rename_to: Option<String>,
    pub warnings: Vec<String>,
}

impl ScanItem {
    pub fn needs_rename(&self) -> bool {
        self.rename_to.is_some()
    }
}

/// Transliterate to printable ASCII; the Neuros can't display anything else.
pub fn sanitize_text(s: &str) -> String {
    let t = deunicode::deunicode(s);
    let cleaned: String = t
        .chars()
        .map(|c| {
            if (' '..='~').contains(&c) {
                c
            } else if c.is_whitespace() {
                ' '
            } else {
                '_'
            }
        })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// ASCII-safe FAT file name component.
pub fn sanitize_filename(s: &str) -> String {
    let t: String = sanitize_text(s).chars().map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c }).collect();
    let t = t.trim_end_matches(['.', ' ']).to_string();
    if t.is_empty() { "_".into() } else { t }
}

fn is_ascii_name(s: &str) -> bool {
    s.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// Normalise a user-supplied relative path ("/MUSIC/", "MUSIC\\x" → "MUSIC", "MUSIC/x").
pub fn normalize_rel(s: &str) -> String {
    s.split(['/', '\\']).filter(|c| !c.is_empty() && *c != ".").collect::<Vec<_>>().join("/")
}

pub fn device_path(drive: Drive, rel: &str) -> String {
    format!("{}/{}", drive.prefix(), rel)
}

fn audio_type(name: &str) -> Option<FileType> {
    let ext = name.rsplit_once('.')?.1;
    if !EXTENSIONS.iter().any(|x| ext.eq_ignore_ascii_case(x)) {
        return None;
    }
    FileType::from_ext(ext.to_ascii_lowercase())
}

fn guess_recording(file_name: &str, existing: &HashMap<String, String>, dev_path: &str) -> String {
    if let Some(r) = existing.get(&dev_path.to_lowercase()) {
        return r.clone();
    }
    if file_name.to_lowercase().starts_with("mic") { "Microphone".into() } else { "FM Radio".into() }
}

/// Tags and duration of one audio file, already sanitised to ASCII.
#[derive(Debug, Clone, Default)]
pub struct TagInfo {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub track_no: Option<u32>,
    pub disc: Option<u32>,
    pub duration_secs: u32,
    pub warnings: Vec<String>,
}

/// Read tags from any reader.  `file_name` is used for the title fallback.
pub fn read_tags(
    reader: Result<Box<dyn vfs::ReadSeek + '_>>,
    ft: FileType,
    file_name: &str,
    placeholders: bool,
) -> TagInfo {
    let stem = file_name.rsplit_once('.').map(|(s, _)| s).unwrap_or(file_name);
    let mut t = TagInfo::default();
    let parsed =
        reader.map_err(|e| e.to_string()).and_then(|r| Probe::with_file_type(r, ft).read().map_err(|e| e.to_string()));
    let mut title = None;
    match parsed {
        Ok(f) => {
            t.duration_secs = f.properties().duration().as_secs_f64().round() as u32;
            if t.duration_secs == 0 {
                t.warnings.push("could not determine duration".into());
            }
            if let Some(tag) = f.primary_tag().or_else(|| f.first_tag()) {
                // ASCII-safe, and "?" left by Latin-1 taggers repaired.
                let clean = |v: Option<std::borrow::Cow<'_, str>>| {
                    v.map(|s| crate::textfix::repair(&sanitize_text(&s))).filter(|s| !s.is_empty())
                };
                title = clean(tag.title());
                t.artist = clean(tag.artist());
                t.album = clean(tag.album());
                t.genre = clean(tag.genre());
                t.track_no = tag.track();
                t.disc = tag.disk();
            } else {
                t.warnings.push("no tags; using file name as title".into());
            }
        }
        Err(e) => t.warnings.push(format!("could not read file: {e}")),
    }
    if placeholders {
        t.artist.get_or_insert_with(|| "Unknown Artist".into());
        t.album.get_or_insert_with(|| "Unknown Album".into());
        t.genre.get_or_insert_with(|| "Unknown Genre".into());
    }
    t.title = title.unwrap_or_else(|| sanitize_text(stem));
    t
}

/// Audio type from a file name, if it's one the Neuros plays.
pub fn audio_file_type(name: &str) -> Option<FileType> {
    audio_type(name)
}

/// Read tags/duration for one file and build its record.
fn read_track(
    vfs: &dyn Vfs,
    rel: &str,
    size: u64,
    ft: FileType,
    drive: Drive,
    opts: &ScanOptions,
    recording: Option<String>,
) -> ScanItem {
    let tags = read_tags(vfs.open_read(rel), ft, vfs::file_name(rel), opts.placeholders && recording.is_none());
    let TagInfo { title, artist, album, genre, track_no, disc, duration_secs: secs, mut warnings } = tags;

    let rename_to = if rel.split('/').all(is_ascii_name) {
        None
    } else {
        warnings.push("file name has non-ASCII characters (locks up the Neuros); needs renaming".into());
        Some(rel.split('/').map(sanitize_filename).collect::<Vec<_>>().join("/"))
    };
    let final_rel = rename_to.as_deref().unwrap_or(rel);

    ScanItem {
        track: Track {
            title,
            artist,
            album,
            genre,
            recording,
            disc,
            track_no,
            duration_secs: secs,
            size_kb: (size / 1024) as u32,
            device_path: device_path(drive, final_rel),
        },
        rel_path: rel.to_string(),
        rename_to,
        warnings,
    }
}

fn is_record_path(rel: &str) -> bool {
    rel.split('/').next().is_some_and(|c| c.eq_ignore_ascii_case(RECORD_DIR))
}

fn walk(
    vfs: &dyn Vfs,
    dir: &str,
    include_recordings: bool,
    cancel: &AtomicBool,
    out: &mut Vec<(String, u64, FileType)>,
) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Other("scan cancelled".into()));
    }
    for e in vfs.read_dir(dir)? {
        let p = vfs::join(dir, &e.name);
        if e.is_dir {
            let lower = e.name.to_lowercase();
            if lower.starts_with('.') || SKIP_DIRS.contains(&lower.as_str()) {
                continue;
            }
            if !include_recordings && is_record_path(&p) {
                continue;
            }
            walk(vfs, &p, include_recordings, cancel, out)?;
        } else if let Some(ft) = audio_type(&e.name) {
            out.push((p, e.len, ft));
        }
    }
    Ok(())
}

/// Scan the device.  `existing_recordings` maps lower-cased device paths to
/// the recording type from the current database, so rebuilds keep it.
/// `progress(done, total)` is called as files are read.
pub fn scan(
    vfs: &dyn Vfs,
    drive: Drive,
    opts: &ScanOptions,
    existing_recordings: &HashMap<String, String>,
    cancel: &AtomicBool,
    mut progress: impl FnMut(usize, usize),
) -> Result<Vec<ScanItem>> {
    // Resolve the music dir case-insensitively, component by component.
    let mut music = String::new();
    for part in normalize_rel(&opts.music_dir).split('/').filter(|p| !p.is_empty()) {
        music = vfs::find_dir_ci(vfs, &music, part)
            .ok_or_else(|| Error::Other(format!("folder {} not found on the device", opts.music_dir)))?;
    }

    let mut files = Vec::new();
    walk(vfs, &music, opts.include_recordings, cancel, &mut files)?;
    // Scanning a subfolder: still pick up recordings from the root.
    if opts.include_recordings
        && !music.is_empty()
        && !is_record_path(&music)
        && let Some(rec) = vfs::find_dir_ci(vfs, "", RECORD_DIR)
    {
        walk(vfs, &rec, true, cancel, &mut files)?;
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files.dedup_by(|a, b| a.0 == b.0);

    let total = files.len();
    let mut out = Vec::with_capacity(total);
    for (i, (rel, size, ft)) in files.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Other("scan cancelled".into()));
        }
        let recording = is_record_path(rel)
            .then(|| guess_recording(vfs::file_name(rel), existing_recordings, &device_path(drive, rel)));
        out.push(read_track(vfs, rel, *size, *ft, drive, opts, recording));
        progress(i + 1, total);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize_text("Björk  –  Jóga"), "Bjork - Joga");
        assert_eq!(sanitize_filename("AC/DC: Live?"), "AC_DC_ Live_");
        assert_eq!(device_path(Drive::C, "MUSIC/a b/c.mp3"), "C:/MUSIC/a b/c.mp3");
        assert_eq!(normalize_rel("/MUSIC\\x/./"), "MUSIC/x");
    }
}
