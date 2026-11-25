//! Find duplicate songs on the Neuros and remove the extra copies.
//!
//! How a copy relates to the one kept:
//! - **identical**: same size and same audio (fingerprint of the middle of
//!   the file), e.g. the same file copied twice;
//! - **same album**: same artist, title and album (and track number from
//!   the file name, if both have one) with lengths within 3 s — e.g. two rips of one album, or
//!   a tagged and an untagged copy;
//! - **other album** (only if asked for): the same song on another album,
//!   e.g. a best-of.
//!
//! Titles are compared ignoring case, punctuation and bracketed parts like
//! "(Remastered)", but "(live)", "(remix)", "(demo)" etc. keep versions
//! apart.  Two files in one folder are never taken as copies unless
//! identical.  For untagged files the artist and title are taken from the
//! file name and folders ("1 - Artist - Title.mp3").
//!
//! Works from the current database plus the file sizes on the device; only
//! files of equal size are read.  Recordings are left out.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::device::{DeviceInfo, WriteOptions, WriteReport, commit, remove_empty_parents};
use crate::scan::sanitize_text;
use crate::vfs::{self, Vfs};
use crate::{Error, Result, Track};

/// Durations within this many seconds count as the same song.
const SAME_LEN_SECS: u32 = 3;

/// How a copy relates to the suggested keeper of its group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Relation {
    Keeper,
    Identical,
    SameAlbum,
    OtherAlbum,
}

impl Relation {
    pub fn label(self) -> &'static str {
        match self {
            Relation::Keeper => "keep",
            Relation::Identical => "identical",
            Relation::SameAlbum => "same album",
            Relation::OtherAlbum => "other album",
        }
    }
    /// Suggested for deletion.
    pub fn suggest_delete(self) -> bool {
        matches!(self, Relation::Identical | Relation::SameAlbum)
    }
}

#[derive(Debug, Clone)]
pub struct DupFile {
    /// Device-relative path as on the device.
    pub path: String,
    pub size: u64,
    pub track: Track,
    /// Audio bitrate estimate in kbit/s (0 if the length is unknown).
    pub kbps: u32,
    /// Has a real artist tag (not taken from the file name).
    pub tagged: bool,
    pub relation: Relation,
}

#[derive(Debug, Clone)]
pub struct DupGroup {
    /// Display name, "Artist - Title".
    pub name: String,
    pub files: Vec<DupFile>,
    /// Index into `files` of the suggested copy to keep.
    pub keep: usize,
}

impl DupGroup {
    /// The copies that aren't the suggested keeper.
    pub fn extras(&self) -> impl Iterator<Item = &DupFile> {
        self.files.iter().enumerate().filter(move |(i, _)| *i != self.keep).map(|(_, f)| f)
    }
}

fn is_unknown(s: Option<&str>) -> bool {
    s.is_none_or(|s| s.is_empty() || s.eq_ignore_ascii_case("unknown artist"))
}

/// Words in brackets that make a different version of a song.
const VERSION_WORDS: &[&str] = &[
    "live",
    "remix",
    "mix",
    "rmx",
    "acoustic",
    "demo",
    "instrumental",
    "edit",
    "version",
    "unplugged",
    "session",
    "reprise",
    "cover",
    "dub",
    "extended",
    "radio",
    "karaoke",
    "reconstruction",
    "mono",
    "rehearsal",
];

/// Comparison form: lower-case letters and digits only; bracketed parts are
/// dropped unless they name a version ("(live)", "[demo]").
pub fn norm(s: &str) -> String {
    let s = sanitize_text(s).to_lowercase();
    let mut out = String::new();
    let mut inner = String::new();
    let mut depth = 0i32;
    let keep_inner =
        |inner: &str| inner.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| VERSION_WORDS.contains(&w));
    for c in s.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    if keep_inner(&inner) {
                        out.extend(inner.chars().filter(char::is_ascii_alphanumeric));
                    }
                    inner.clear();
                }
            }
            c if depth > 0 => inner.push(c),
            c if c.is_ascii_alphanumeric() => out.push(c),
            _ => {}
        }
    }
    if depth > 0 && keep_inner(&inner) {
        out.extend(inner.chars().filter(char::is_ascii_alphanumeric));
    }
    if out.is_empty() { s.chars().filter(|c| c.is_ascii_alphanumeric()).collect() } else { out }
}

/// Album in comparison form: the album tag, or the folder for untagged
/// files; a trailing year ("All Ages - 1995") is ignored.
fn album_key(t: &Track, path: &str) -> String {
    let album = t.album.as_deref().filter(|a| !a.eq_ignore_ascii_case("unknown album"));
    let raw = album.map(str::to_string).unwrap_or_else(|| vfs::file_name(vfs::parent(path)).to_string());
    let mut k = norm(&raw);
    let digits = k.len() - k.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 4 && k.len() > 4 {
        k.truncate(k.len() - 4);
    }
    k
}

/// Track number from the start of the file name ("10._Title.mp3",
/// "03 - Artist - Title.mp3").  The database has no real track numbers.
fn file_track_no(path: &str) -> Option<u32> {
    let name = vfs::file_name(path);
    let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
    let rest = &name[digits.len()..];
    (!digits.is_empty() && digits.len() <= 3 && rest.starts_with(['.', '_', '-', ' ']))
        .then(|| digits.parse().ok())
        .flatten()
}

fn same_album(a: &DupFile, b: &DupFile) -> bool {
    same_album_of(&a.track, &a.path, &b.track, &b.path)
}

fn same_album_of(a: &Track, a_path: &str, b: &Track, b_path: &str) -> bool {
    if let (Some(x), Some(y)) = (file_track_no(a_path), file_track_no(b_path))
        && x != y
    {
        return false;
    }
    let (ka, kb) = (album_key(a, a_path), album_key(b, b_path));
    let (short, long) = if ka.len() <= kb.len() { (&ka, &kb) } else { (&kb, &ka) };
    // Folder names are often cut short ("Punk Rock Songs The Epic Year").
    ka == kb || (short.len() >= 6 && long.starts_with(short.as_str()))
}

fn is_track_number(s: &str) -> bool {
    let s = s.trim().trim_end_matches('.');
    !s.is_empty() && s.len() <= 3 && s.chars().all(|c| c.is_ascii_digit())
}

/// Strip a leading track number: "01 Title", "1. Title", "01_Title".
fn strip_number(s: &str) -> &str {
    let t = s.trim_start();
    let digits = t.len() - t.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 || digits > 3 {
        return s;
    }
    let rest = &t[digits..];
    let after = rest.trim_start_matches(['.', '_', '-', ' ', ')']);
    if after.len() == rest.len() || after.is_empty() { s } else { after }
}

/// (artist, title) in comparison form.  For untagged files, guessed from
/// the title (file name) and the folders of `path`.
fn song_key(t: &Track, path: &str) -> Option<(String, String)> {
    if !is_unknown(t.artist.as_deref()) {
        let title = norm(&t.title);
        return (!title.is_empty()).then(|| (norm(t.artist.as_deref().unwrap()), title));
    }
    let folders: Vec<String> = path.split('/').rev().skip(1).map(norm).collect();
    let parts: Vec<&str> = t.title.split(" - ").map(str::trim).filter(|p| !is_track_number(p)).collect();
    // "Artist - Title" where Artist is one of the folders, in either order.
    let (artist, title) = match parts.iter().position(|p| folders.contains(&norm(p))) {
        Some(i) if parts.len() >= 2 => {
            let title: Vec<&str> = parts.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, p)| *p).collect();
            (norm(parts[i]), norm(strip_number(&title.join(" "))))
        }
        // Otherwise the artist folder: MUSIC/<Artist>/...
        _ => {
            let comps: Vec<&str> = path.split('/').collect();
            let artist = if comps.len() >= 3 { norm(comps[1]) } else { return None };
            (artist, norm(strip_number(&t.title)))
        }
    };
    (!artist.is_empty() && !title.is_empty()).then_some((artist, title))
}

/// Union-find that also tracks the folders in each group.
/// Same song from the same album with lengths within 3 s (the "same
/// album" rule above, for sync).  Paths are device-relative.
pub(crate) fn same_album_song(a: &Track, a_path: &str, b: &Track, b_path: &str) -> bool {
    a.duration_secs > 0
        && b.duration_secs > 0
        && a.duration_secs.abs_diff(b.duration_secs) <= SAME_LEN_SECS
        && song_key(a, a_path).is_some_and(|k| Some(k) == song_key(b, b_path))
        && same_album_of(a, a_path, b, b_path)
}

/// `song_key` for indexing.
pub(crate) fn song_key_of(t: &Track, path: &str) -> Option<(String, String)> {
    song_key(t, path)
}

struct Uf {
    parent: Vec<usize>,
    dirs: Vec<std::collections::HashSet<String>>,
}
impl Uf {
    fn new(dirs: Vec<String>) -> Self {
        Self { parent: (0..dirs.len()).collect(), dirs: dirs.into_iter().map(|d| [d].into()).collect() }
    }
    fn find(&mut self, i: usize) -> usize {
        let p = self.parent[i];
        if p == i {
            return i;
        }
        let r = self.find(p);
        self.parent[i] = r;
        r
    }
    /// Join the groups of `a` and `b`; with `distinct_dirs`, only if no
    /// folder would then hold two of the group's files.
    fn union(&mut self, a: usize, b: usize, distinct_dirs: bool) {
        let (a, b) = (self.find(a), self.find(b));
        if a == b || (distinct_dirs && !self.dirs[a].is_disjoint(&self.dirs[b])) {
            return;
        }
        let moved = std::mem::take(&mut self.dirs[b]);
        self.dirs[a].extend(moved);
        self.parent[b] = a;
    }
}

/// Find duplicates among the tracks in the device's database.  With
/// `other_albums`, also the same song on different albums.
/// `progress(done, total)` over the files that have to be read.
pub fn find_duplicates(
    vfs: &dyn Vfs,
    info: &DeviceInfo,
    other_albums: bool,
    cancel: &AtomicBool,
    mut progress: impl FnMut(usize, usize),
) -> Result<Vec<DupGroup>> {
    let db =
        info.existing.as_ref().ok_or_else(|| Error::Other("no database on the device; Scan + Write first".into()))?;
    let mut on_device = Vec::new();
    crate::sync::walk_device(vfs, "", &mut on_device)?;
    let sizes: HashMap<String, (String, u64)> =
        on_device.into_iter().map(|(p, s)| (p.to_lowercase(), (p, s))).collect();

    let mut files: Vec<DupFile> = Vec::new();
    for t in db.tracks().iter().map(|t| t.to_track()) {
        if t.recording.is_some() {
            continue;
        }
        let rel = t.device_path.get(3..).unwrap_or("").to_lowercase();
        let Some((path, size)) = sizes.get(&rel) else { continue };
        let kbps = if t.duration_secs > 0 { (size * 8 / 1000 / t.duration_secs as u64) as u32 } else { 0 };
        let tagged = !is_unknown(t.artist.as_deref());
        files.push(DupFile { path: path.clone(), size: *size, track: t, kbps, tagged, relation: Relation::Keeper });
    }
    let mut uf = Uf::new(files.iter().map(|f| vfs::parent(&f.path).to_lowercase()).collect());

    // Same recording: equal size, equal fingerprint.
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        by_size.entry(f.size).or_default().push(i);
    }
    let to_read: Vec<usize> = by_size.values().filter(|v| v.len() > 1).flatten().copied().collect();
    let mut fp: HashMap<usize, u64> = HashMap::new();
    for (n, &i) in to_read.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Other("cancelled".into()));
        }
        if let Ok(mut r) = vfs.open_read(&files[i].path)
            && let Ok(h) = crate::sync::fingerprint(&mut *r, files[i].size)
        {
            fp.insert(i, h);
        }
        progress(n + 1, to_read.len());
    }
    let mut by_fp: HashMap<(u64, u64), usize> = HashMap::new();
    for (&i, &h) in &fp {
        match by_fp.get(&(files[i].size, h)) {
            Some(&j) => uf.union(j, i, false),
            None => {
                by_fp.insert((files[i].size, h), i);
            }
        }
    }

    // Same song: same key, lengths close (chained).
    let mut by_key: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        if let Some(k) = song_key(&f.track, &f.path) {
            by_key.entry(k).or_default().push(i);
        }
    }
    // Candidate pairs, best first: same album before other album, then
    // closest length.
    let mut pairs: Vec<(bool, u32, usize, usize)> = Vec::new();
    for idx in by_key.values().filter(|v| v.len() > 1) {
        for (n, &i) in idx.iter().enumerate() {
            for &j in &idx[n + 1..] {
                let (a, b) = (&files[i], &files[j]);
                let diff = a.track.duration_secs.abs_diff(b.track.duration_secs);
                if a.track.duration_secs == 0 || b.track.duration_secs == 0 || diff > SAME_LEN_SECS {
                    continue;
                }
                let sa = same_album(a, b);
                if sa || other_albums {
                    pairs.push((!sa, diff, i, j));
                }
            }
        }
    }
    pairs.sort();
    for (_, _, i, j) in pairs {
        uf.union(i, j, true);
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..files.len() {
        let r = uf.find(i);
        groups.entry(r).or_default().push(i);
    }
    let mut out: Vec<DupGroup> = groups
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|g| {
            let mut members: Vec<DupFile> = g.iter().map(|&i| files[i].clone()).collect();
            members.sort_by_key(|a| a.path.to_lowercase());
            // Keep: real tags, then a real album, then higher bitrate, then
            // the shallower / shorter path.
            let keep = (0..members.len())
                .max_by_key(|&i| {
                    let f = &members[i];
                    let album = f.track.album.as_deref().is_some_and(|a| !a.eq_ignore_ascii_case("unknown album"));
                    (
                        f.tagged,
                        album,
                        f.kbps / 8,
                        std::cmp::Reverse(f.path.matches('/').count()),
                        std::cmp::Reverse(f.path.len()),
                    )
                })
                .unwrap();
            let key_fp = |f: &DupFile| {
                files.iter().position(|x| x.path == f.path).and_then(|i| fp.get(&i)).map(|h| (f.size, *h))
            };
            let kfp = key_fp(&members[keep]);
            let kept = members[keep].clone();
            for (i, m) in members.iter_mut().enumerate() {
                m.relation = if i == keep {
                    Relation::Keeper
                } else if kfp.is_some() && key_fp(m) == kfp {
                    Relation::Identical
                } else if same_album(&kept, m) {
                    Relation::SameAlbum
                } else {
                    Relation::OtherAlbum
                };
            }
            let k = &members[keep].track;
            let name = match &k.artist {
                Some(a) if !is_unknown(Some(a)) => format!("{a} - {}", k.title),
                _ => k.title.clone(),
            };
            DupGroup { name, files: members, keep }
        })
        .collect();
    out.sort_by_key(|g| {
        let strongest = g.files.iter().filter(|f| f.relation != Relation::Keeper).map(|f| f.relation).min();
        (strongest, g.name.to_lowercase())
    });
    Ok(out)
}

/// Delete `paths` (device-relative) from the device and the database.
/// Call `vfs.finish()` afterwards.
pub fn delete_tracks(vfs: &dyn Vfs, info: &DeviceInfo, paths: &[String], opts: &WriteOptions) -> Result<WriteReport> {
    let db = info.existing.as_ref().ok_or_else(|| Error::Other("no database on the device".into()))?;
    let drive = opts.drive.unwrap_or(info.drive);
    let tracks: Vec<Track> = db.tracks().iter().map(|t| t.to_track()).collect();
    let gone: std::collections::HashSet<String> =
        paths.iter().map(|p| crate::scan::device_path(drive, p).to_lowercase()).collect();
    commit(vfs, info, tracks, opts, &info.overrides, WriteReport::default(), |tracks, report| {
        for p in paths {
            vfs.remove(p)?;
            remove_empty_parents(vfs, vfs::parent(p).to_string());
            report.deleted.push(p.clone());
        }
        tracks.retain(|t| !gone.contains(&t.device_path.to_lowercase()));
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(title: &str, artist: Option<&str>) -> Track {
        Track {
            title: title.into(),
            artist: artist.map(Into::into),
            album: None,
            genre: None,
            recording: None,
            disc: None,
            track_no: None,
            duration_secs: 100,
            size_kb: 1,
            device_path: String::new(),
        }
    }

    #[test]
    fn keys() {
        let k = |title, artist, path| song_key(&t(title, artist), path);
        let want = Some(("againstme".to_string(), "tsr".to_string()));
        assert_eq!(k("T.S.R. (This Shit Rules)", Some("Against Me!"), "x"), want);
        assert_eq!(
            k(
                "1 - Against Me! - T.S.R.",
                Some("Unknown Artist"),
                "music/Against Me!/Cowboy/1 - Against Me! - T.S.R..mp3"
            ),
            want
        );
        assert_eq!(k("01_T.S.R.", None, "music/Against Me!/Cowboy/01_T.S.R..mp3"), want);
        assert_eq!(k("Speak To Me - The Flaming Lips", None, "music/The Flaming Lips/x.mp3").unwrap().1, "speaktome");
        assert_eq!(k("1979", Some("Smashing Pumpkins"), "x").unwrap().1, "1979");
        assert_eq!(k("song", None, "song.mp3"), None);
        assert_ne!(k("Cease (live)", Some("BR"), "x"), k("Cease", Some("BR"), "x"));
        assert_eq!(k("Cease (Remastered 2004)", Some("BR"), "x"), k("Cease", Some("BR"), "x"));
        assert_eq!(norm("No One Knows (UNKLE remix)"), "nooneknowsunkleremix");
    }
}
