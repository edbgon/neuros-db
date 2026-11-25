//! One-way sync of a music folder on the PC to a folder on the Neuros.
//!
//! Files are paired by relative path (made ASCII-safe for the Neuros, as
//! in `add`), then by contents, then by tags (see `plan_sync`); paired
//! files are compared by size.  New and changed files are copied; files
//! only on the Neuros are removed if asked.  The database is then updated:
//! tracks outside the synced folder keep their records, synced tracks get
//! fresh records from the PC tags (so re-tagged files update too).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::device::{DeviceInfo, WriteOptions, WriteReport, base_tracks, commit, remove_empty_parents, track_from};
use crate::scan::{TagInfo, audio_file_type, normalize_rel, read_tags, sanitize_filename};
use crate::vfs::{self, Vfs};
use crate::{Error, Result, ScanOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAction {
    /// Not on the Neuros yet.
    Copy,
    /// On the Neuros with a different size.
    Update,
    /// Same size on both: not copied, but its record is refreshed from the PC tags.
    Unchanged,
    /// A second copy on the PC of a song already on the Neuros (or already
    /// in this sync) from the same album: not copied.  See `dup_of`.
    Skip,
}

/// How a PC file was paired with a file on the Neuros.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// Not on the Neuros (new).
    None,
    /// Same relative path.
    Path,
    /// Same contents under another name (renamed by an older tool).
    Content,
    /// Same artist, album and title in the database (re-tagged or re-encoded).
    Tags,
}

#[derive(Debug, Clone)]
pub struct SyncEntry {
    pub action: SyncAction,
    pub matched: Match,
    pub host: PathBuf,
    pub size: u64,
    /// Device-relative destination.  For files already on the Neuros under
    /// another name, that existing name (they are not renamed).
    pub dest: String,
    pub tags: TagInfo,
    /// For `Skip`: the Neuros path of the copy that is kept.
    pub dup_of: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SyncPlan {
    pub pc_root: PathBuf,
    /// Device folder synced into (device-relative).
    pub device_dir: String,
    pub entries: Vec<SyncEntry>,
    /// Audio files in the device folder that aren't on the PC: (path, size).
    pub extra: Vec<(String, u64)>,
}

impl SyncPlan {
    pub fn count(&self, a: SyncAction) -> (usize, u64) {
        self.entries.iter().filter(|e| e.action == a).fold((0, 0), |(n, b), e| (n + 1, b + e.size))
    }
    pub fn matched(&self, m: Match) -> usize {
        self.entries.iter().filter(|e| e.matched == m).count()
    }
    pub fn extra_bytes(&self) -> u64 {
        self.extra.iter().map(|(_, s)| s).sum()
    }
}

pub(crate) fn walk_device(vfs: &dyn Vfs, dir: &str, out: &mut Vec<(String, u64)>) -> Result<()> {
    for e in vfs.read_dir(dir)? {
        let p = vfs::join(dir, &e.name);
        if e.is_dir {
            let lower = e.name.to_lowercase();
            if !lower.starts_with('.') && !lower.starts_with("woid_") {
                walk_device(vfs, &p, out)?;
            }
        } else if audio_file_type(&e.name).is_some() {
            out.push((p, e.len));
        }
    }
    Ok(())
}

const FP_CHUNK: u64 = 16 * 1024;

/// Cheap content fingerprint: a hash of 16 KiB from the middle of the file
/// (audio data, so tag-only edits that keep the size still match).  Files
/// of equal size whose middles match are taken to be the same recording.
pub fn fingerprint(r: &mut dyn vfs::ReadSeek, size: u64) -> std::io::Result<u64> {
    use std::hash::Hasher;
    use std::io::{Read, SeekFrom};
    let start = if size > 2 * FP_CHUNK { (size / 2) & !4095 } else { 0 };
    r.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity(FP_CHUNK as usize);
    r.take(FP_CHUNK).read_to_end(&mut buf)?;
    let mut h = std::hash::DefaultHasher::new();
    h.write(&buf);
    Ok(h.finish())
}

fn host_fingerprint(p: &Path, size: u64) -> Option<u64> {
    let mut f = std::fs::File::open(p).ok()?;
    fingerprint(&mut f, size).ok()
}

/// Matching key for tags: lower-cased, ASCII-safe artist / album / title.
fn tag_key(title: &str, artist: Option<&str>, album: Option<&str>) -> Option<(String, String, String)> {
    let norm = |s: &str| crate::scan::sanitize_text(s).to_lowercase();
    let t = norm(title);
    let a = norm(artist.unwrap_or(""));
    (!t.is_empty() && !a.is_empty()).then(|| (a, norm(album.unwrap_or("")), t))
}

/// Compare `pc_root` with `device_dir` on the Neuros.
///
/// PC files are paired with Neuros files by, in order: relative path,
/// contents (size + fingerprint, for files an older tool renamed), and
/// artist/album/title from `known` (the current database tracks, for files
/// re-tagged on the PC).  Reads tags of all PC files (local, fast) and a
/// small chunk of each size-matched Neuros file.  With `skip_dupes`, new
/// PC files that are the same song from the same album as one already
/// paired (or already being copied) are skipped (see `dedupe`).
/// `progress(done, total)`.
#[allow(clippy::too_many_arguments)]
pub fn plan_sync(
    vfs: &dyn Vfs,
    known: &[crate::Track],
    pc_root: &Path,
    device_dir: &str,
    placeholders: bool,
    skip_dupes: bool,
    cancel: &AtomicBool,
    mut progress: impl FnMut(usize, usize),
) -> Result<SyncPlan> {
    if !pc_root.is_dir() {
        return Err(Error::Other(format!("{} is not a folder", pc_root.display())));
    }
    let cancelled = || Error::Other("cancelled".into());
    // Resolve the device folder case-insensitively; it may not exist yet.
    let mut dir = String::new();
    let mut exists = true;
    for part in normalize_rel(device_dir).split('/').filter(|p| !p.is_empty()) {
        match exists.then(|| vfs::find_dir_ci(vfs, &dir, part)).flatten() {
            Some(p) => dir = p,
            None => {
                exists = false;
                dir = vfs::join(&dir, &sanitize_filename(part));
            }
        }
    }
    let mut on_device = Vec::new();
    if exists {
        walk_device(vfs, &dir, &mut on_device)?;
    }
    let mut device_map: HashMap<String, (String, u64)> =
        on_device.into_iter().map(|(p, s)| (p.to_lowercase(), (p, s))).collect();

    let files = crate::device::collect_audio_files(pc_root);
    let mut total = files.len();
    let mut used: HashSet<String> = HashSet::new();
    let mut entries = Vec::with_capacity(total);

    // 1. Same path.
    for (i, host) in files.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        let rel = host.strip_prefix(pc_root).unwrap_or(host);
        let parts: Vec<String> = rel.iter().map(|c| sanitize_filename(&c.to_string_lossy())).collect();
        let mut dest = vfs::join(&dir, &parts.join("/"));
        // Two PC names can map to the same ASCII name.
        let mut n = 2;
        while used.contains(&dest.to_lowercase()) {
            let base = vfs::join(&dir, &parts.join("/"));
            let (stem, ext) = base
                .rsplit_once('.')
                .map(|(s, e)| (s.to_string(), format!(".{e}")))
                .unwrap_or((base.clone(), String::new()));
            dest = format!("{stem} ({n}){ext}");
            n += 1;
        }
        used.insert(dest.to_lowercase());

        let size = std::fs::metadata(host).map(|m| m.len()).unwrap_or(0);
        let name = host.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let ft = audio_file_type(&name).expect("collected audio files only");
        let reader = std::fs::File::open(host)
            .map(|f| Box::new(std::io::BufReader::new(f)) as Box<dyn vfs::ReadSeek>)
            .map_err(Error::from);
        let tags = read_tags(reader, ft, &name, placeholders);
        let (action, matched) = match device_map.remove(&dest.to_lowercase()) {
            Some((real, s)) => {
                dest = real; // keep the case already on the device
                (if s == size { SyncAction::Unchanged } else { SyncAction::Update }, Match::Path)
            }
            None => (SyncAction::Copy, Match::None),
        };
        entries.push(SyncEntry { action, matched, host: host.clone(), size, dest, tags, dup_of: None });
        progress(i + 1, total);
    }

    // 2. Same contents under another name.
    let mut by_size: HashMap<u64, Vec<String>> = HashMap::new();
    for (k, (_, s)) in &device_map {
        by_size.entry(*s).or_default().push(k.clone());
    }
    for v in by_size.values_mut() {
        v.sort();
    }
    let pending: Vec<usize> = (0..entries.len())
        .filter(|&i| entries[i].matched == Match::None && by_size.contains_key(&entries[i].size))
        .collect();
    let base = total;
    total += pending.len();
    let mut dev_fp: HashMap<String, Option<u64>> = HashMap::new();
    let rel_of = |device_path: &str| device_path.get(3..).unwrap_or("").to_lowercase(); // strip "C:/"
    let known_by_rel: HashMap<String, &crate::Track> = known.iter().map(|t| (rel_of(&t.device_path), t)).collect();
    let mut pc_sizes: HashMap<u64, usize> = HashMap::new();
    for &i in &pending {
        *pc_sizes.entry(entries[i].size).or_default() += 1;
    }
    for (n, &i) in pending.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        progress(base + n + 1, total);
        let size = entries[i].size;
        let Some(cands) = by_size.get(&size) else { continue };
        // Fast path: a size that is unique on both sides, and the database
        // record of that Neuros file has the same duration and title.  Reading from
        // the Neuros is slow (tens of ms per file over its USB), so only
        // read when this doesn't settle it.
        let unique = cands.len() == 1 && pc_sizes.get(&size) == Some(&1);
        let same_record = |k: &String| {
            let e = &entries[i];
            known_by_rel.get(k).is_some_and(|t| {
                t.duration_secs.abs_diff(e.tags.duration_secs) <= 2
                    && crate::scan::sanitize_text(&t.title)
                        .eq_ignore_ascii_case(&crate::scan::sanitize_text(&e.tags.title))
            })
        };
        let hit = if unique && same_record(&cands[0]) {
            Some(0)
        } else {
            let Some(pc_fp) = host_fingerprint(&entries[i].host, size) else { continue };
            cands.iter().position(|k| {
                *dev_fp.entry(k.clone()).or_insert_with(|| {
                    let real = &device_map[k].0;
                    vfs.open_read(real).ok().and_then(|mut r| fingerprint(&mut *r, size).ok())
                }) == Some(pc_fp)
            })
        };
        if let Some(pos) = hit {
            let cands = by_size.get_mut(&size).unwrap();
            let k = cands.remove(pos);
            if cands.is_empty() {
                by_size.remove(&size);
            }
            let (real, _) = device_map.remove(&k).unwrap();
            let e = &mut entries[i];
            e.action = SyncAction::Unchanged;
            e.matched = Match::Content;
            e.dest = real;
        }
    }

    // 3. Same artist/album/title in the database (file changed on the PC).
    // Only pairs that are unique on both sides.
    let mut dev_keys: HashMap<(String, String, String), Vec<String>> = HashMap::new();
    for t in known {
        let k = rel_of(&t.device_path);
        if device_map.contains_key(&k)
            && let Some(key) = tag_key(&t.title, t.artist.as_deref(), t.album.as_deref())
        {
            dev_keys.entry(key).or_default().push(k);
        }
    }
    let mut pc_keys: HashMap<(String, String, String), Vec<usize>> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        if e.matched == Match::None
            && let Some(key) = tag_key(&e.tags.title, e.tags.artist.as_deref(), e.tags.album.as_deref())
        {
            pc_keys.entry(key).or_default().push(i);
        }
    }
    for (key, idx) in pc_keys {
        if let ([i], Some([k])) = (idx.as_slice(), dev_keys.get(&key).map(Vec::as_slice))
            && let Some((real, _)) = device_map.remove(k)
        {
            let e = &mut entries[*i];
            e.action = SyncAction::Update;
            e.matched = Match::Tags;
            e.dest = real;
        }
    }

    // 4. Second copies on the PC of songs that will be on the Neuros anyway.
    if skip_dupes {
        let as_track = |e: &SyncEntry| crate::device::track_from(&e.tags, e.size, String::new());
        let mut present: HashMap<(String, String), Vec<(crate::Track, String)>> = HashMap::new();
        let add = |present: &mut HashMap<_, Vec<_>>, t: crate::Track, path: String| {
            if let Some(k) = crate::dedupe::song_key_of(&t, &path) {
                present.entry(k).or_default().push((t, path));
            }
        };
        for e in entries.iter().filter(|e| e.matched != Match::None) {
            add(&mut present, as_track(e), e.dest.clone());
        }
        for e in entries.iter_mut().filter(|e| e.matched == Match::None) {
            let t = as_track(e);
            let hit = crate::dedupe::song_key_of(&t, &e.dest).and_then(|k| {
                present
                    .get(&k)?
                    .iter()
                    .find(|(o, p)| crate::dedupe::same_album_song(&t, &e.dest, o, p))
                    .map(|(_, p)| p.clone())
            });
            match hit {
                Some(p) => {
                    e.action = SyncAction::Skip;
                    e.dup_of = Some(p);
                }
                None => add(&mut present, t, e.dest.clone()),
            }
        }
    }

    let mut extra: Vec<(String, u64)> = device_map.into_values().collect();
    extra.sort();
    Ok(SyncPlan { pc_root: pc_root.to_path_buf(), device_dir: dir, entries, extra })
}

/// Carry out a sync plan and update the database.  `progress(bytes_done,
/// bytes_total)` while copying.  Call `vfs.finish()` afterwards.
pub fn apply_sync(
    vfs: &dyn Vfs,
    info: &DeviceInfo,
    plan: &SyncPlan,
    delete_extra: bool,
    opts: &WriteOptions,
    fallback: &ScanOptions,
    mut progress: impl FnMut(u64, u64),
) -> Result<WriteReport> {
    let drive = opts.drive.unwrap_or(info.drive);
    let mut report = WriteReport::default();
    let base = base_tracks(vfs, info, drive, fallback, &mut report)?;
    let copies = || plan.entries.iter().filter(|e| matches!(e.action, SyncAction::Copy | SyncAction::Update));
    let total: u64 = copies().map(|e| e.size).sum();

    commit(vfs, info, base, opts, &info.overrides, report, |tracks, report| {
        let mut done = 0;
        for e in copies() {
            vfs::create_dir_all(vfs, vfs::parent(&e.dest))?;
            vfs.write_file(&e.dest, &std::fs::read(&e.host)?)?;
            report.added.push(e.dest.clone());
            done += e.size;
            progress(done, total);
        }
        let mut gone: HashSet<String> = HashSet::new();
        if delete_extra {
            for (p, _) in &plan.extra {
                vfs.remove(p)?;
                remove_empty_parents(vfs, vfs::parent(p).to_string());
                gone.insert(crate::scan::device_path(drive, p).to_lowercase());
                report.deleted.push(p.clone());
            }
        }
        // Synced files get fresh records from the PC; everything else stays.
        let kept = || plan.entries.iter().filter(|e| e.action != SyncAction::Skip);
        let synced: HashSet<String> = kept().map(|e| crate::scan::device_path(drive, &e.dest).to_lowercase()).collect();
        tracks.retain(|t| {
            let k = t.device_path.to_lowercase();
            !synced.contains(&k) && !gone.contains(&k)
        });
        for e in kept() {
            tracks.push(track_from(&e.tags, e.size, crate::scan::device_path(drive, &e.dest)));
        }
        Ok(())
    })
}
