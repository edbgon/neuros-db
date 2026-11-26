use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use neuros_core::device::default_backup_dir;
use neuros_core::reader::load_database;
use neuros_core::vfs::{self, Vfs};
use neuros_core::{BuildOptions, DeviceInfo, Drive, ScanItem, ScanOptions, Source, SwapMode, WriteOptions};

#[derive(Parser)]
#[command(name = "neuros-db", version, about = "Build the database for a Neuros audio player")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Swap {
    /// Detect from the boot signature.
    Auto,
    /// Byte-swapped (Neuros hard disk on a USB-IDE adapter).
    On,
    /// Normal byte order.
    Off,
}

#[derive(Args, Clone)]
struct Target {
    /// Mounted device folder, or a raw disk / image (e.g. /dev/sdc) for a
    /// Neuros hard disk connected through a USB-IDE adapter.
    target: PathBuf,
    /// Byte order for raw disks.
    #[arg(long, value_enum, default_value_t = Swap::Auto)]
    swap: Swap,
}

impl Target {
    fn source(&self) -> Source {
        if self.target.is_dir() {
            Source::Mounted(self.target.clone())
        } else {
            let swap = match self.swap {
                Swap::Auto => SwapMode::Auto,
                Swap::On => SwapMode::Swapped,
                Swap::Off => SwapMode::Normal,
            };
            Source::Raw { path: self.target.clone(), swap }
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// List attached disks that could be a directly-connected Neuros drive.
    Disks,
    /// Show what is known about a device.
    Info(Target),
    /// Scan for MP3/Ogg files and list what would go in the database.
    Scan(ScanArgs),
    /// Scan and write a new database to the device.
    Build {
        #[command(flatten)]
        scan: ScanArgs,
        /// Rename files with non-ASCII names instead of skipping them.
        #[arg(long)]
        rename: bool,
        /// Don't back up the current WOID_DB first.
        #[arg(long)]
        no_backup: bool,
        /// Backup directory.
        #[arg(long)]
        backup_dir: Option<PathBuf>,
        /// Sort "The Examples" under T instead of E.
        #[arg(long)]
        keep_the: bool,
        /// Write even if the target doesn't look like a Neuros.
        #[arg(long)]
        force: bool,
        /// Artist/Album menu (firmware >= 2.14): auto = on if the firmware
        /// supports it or the current database already has it.
        #[arg(long, value_enum, default_value_t = Swap::Auto, value_name = "auto|on|off")]
        artist_albums: Swap,
        /// Open read-only, build and verify the database in memory, write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print the tracks in the device's audio database.
    Dump(Target),
    /// Check the device's audio database for consistency.
    Verify(Target),
    /// Copy MP3/Ogg files (or folders) onto the device and update the database.
    Add {
        #[command(flatten)]
        target: Target,
        /// Files or folders on this computer.
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Put everything in this device folder instead of MUSIC/Artist/Album.
        #[arg(long)]
        to: Option<String>,
        /// Keep the folder structure of the folders given (under --to, or
        /// MUSIC) instead of sorting by tags.
        #[arg(long)]
        keep_folders: bool,
        /// Artist/Album menu: auto|on|off.
        #[arg(long, value_enum, default_value_t = Swap::Auto, value_name = "auto|on|off")]
        artist_albums: Swap,
        /// Don't back up the current WOID_DB first.
        #[arg(long)]
        no_backup: bool,
    },
    /// Repair "?" that old taggers left for apostrophes and dashes
    /// ("Don?t" → "Don't", "1990?1999" → "1990-1999") in the device's database.
    FixText {
        #[command(flatten)]
        target: Target,
        /// Only show what would change.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        no_backup: bool,
    },
    /// Find duplicate songs on the device (and optionally delete the extra copies).
    Dupes {
        #[command(flatten)]
        target: Target,
        /// Also list the same song on other albums (best-ofs, compilations).
        #[arg(long)]
        other_albums: bool,
        /// Delete the extra copies (identical and same-album ones; with
        /// --other-albums also those). The suggested copy of each group is kept.
        #[arg(long)]
        delete: bool,
        /// With --delete: show what would be deleted, don't write.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        no_backup: bool,
    },
    /// Make a device folder match a music folder on this computer.
    Sync {
        #[command(flatten)]
        target: Target,
        /// Music folder on this computer.
        pc_folder: PathBuf,
        /// Device folder to sync into.
        #[arg(long, default_value = "MUSIC")]
        to: String,
        /// Also delete audio files in that device folder that aren't on the PC.
        #[arg(long)]
        delete: bool,
        /// Also copy PC files that are second copies of a song (same artist,
        /// title and album) already on the Neuros. By default they're skipped.
        #[arg(long)]
        keep_dupes: bool,
        /// Only show what would happen.
        #[arg(long)]
        dry_run: bool,
        /// Don't back up the current WOID_DB first.
        #[arg(long)]
        no_backup: bool,
    },
    /// Edit database fields of tracks whose title/artist/album/path contains MATCH.
    /// Edits are stored on the device and survive rescans.
    Edit {
        #[command(flatten)]
        target: Target,
        /// Case-insensitive text to find tracks by.
        #[arg(long = "match")]
        pattern: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        artist: Option<String>,
        #[arg(long)]
        album: Option<String>,
        #[arg(long)]
        genre: Option<String>,
        /// Remove stored edits of the matched tracks instead.
        #[arg(long)]
        forget: bool,
        /// Only list the matched tracks.
        #[arg(long)]
        dry_run: bool,
    },
    /// Copy the device's WOID_DB to a folder on this computer.
    Backup {
        #[command(flatten)]
        target: Target,
        /// Destination (default: the standard backup folder).
        dest: Option<PathBuf>,
    },
}

#[derive(Args)]
struct ScanArgs {
    #[command(flatten)]
    target: Target,
    /// Folder to scan, relative to the device root (default: whole device).
    #[arg(long)]
    music: Option<String>,
    /// Force drive letter C (hard disk) or D (flash).
    #[arg(long, value_parser = ["C", "D", "c", "d"])]
    drive: Option<String>,
    /// Leave out the device's own recordings (WOID_RECORD).
    #[arg(long)]
    no_recordings: bool,
    /// Leave artist/album/genre empty instead of "Unknown ...".
    #[arg(long)]
    no_placeholders: bool,
}

/// Accept the music folder as a host path under a mounted device, too.
fn music_rel(src: &Source, music: &Option<String>) -> String {
    let Some(m) = music else { return String::new() };
    if let Source::Mounted(root) = src
        && let Ok(rel) = Path::new(m).strip_prefix(root)
    {
        return neuros_core::scan::normalize_rel(&rel.to_string_lossy());
    }
    neuros_core::scan::normalize_rel(m)
}

fn audio_dir(vfs: &dyn Vfs) -> Result<String> {
    let db = vfs::find_dir_ci(vfs, "", "WOID_DB").context("no WOID_DB on the device")?;
    vfs::find_dir_ci(vfs, &db, "audio").context("no WOID_DB/audio database found")
}

fn do_scan(vfs: &dyn Vfs, info: &mut DeviceInfo, a: &ScanArgs, src: &Source) -> Result<Vec<ScanItem>> {
    if let Some(d) = &a.drive {
        info.drive = if d.eq_ignore_ascii_case("d") { Drive::D } else { Drive::C };
        info.drive_reason = "set on command line".into();
    }
    let opts = ScanOptions {
        music_dir: music_rel(src, &a.music),
        include_recordings: !a.no_recordings,
        placeholders: !a.no_placeholders,
    };
    let cancel = AtomicBool::new(false);
    let items = neuros_core::scan(vfs, info.drive, &opts, &info.existing_recordings(), &cancel, |d, t| {
        if d % 50 == 0 || d == t {
            eprint!("\rreading tags {d}/{t}");
        }
    })?;
    eprintln!();
    Ok(items)
}

fn print_info(info: &DeviceInfo) {
    println!("device:    {}", info.location);
    println!("serial:    {}", info.serial.as_deref().unwrap_or("-"));
    println!("firmware:  {}", info.firmware.as_deref().unwrap_or("-"));
    if let Some(b) = info.total_bytes {
        println!("size:      {} MB", b >> 20);
    }
    println!("drive:     {} ({})", info.drive.prefix(), info.drive_reason);
    println!("art/alb:   {} ({})", if info.artist_albums { "on" } else { "off" }, info.artist_albums_reason);
    match (&info.existing, &info.existing_error) {
        (Some(db), _) => println!("database:  {} tracks", db.tracks().len()),
        (None, Some(e)) => println!("database:  unreadable: {e}"),
        _ => println!("database:  none"),
    }
}

fn fmt_time(s: u32) -> String {
    format!("{}:{:02}", s / 60, s % 60)
}

fn open(t: &Target, writable: bool) -> Result<(Source, Box<dyn Vfs>, DeviceInfo)> {
    let src = t.source();
    let (vfs, probe) = src.open(writable)?;
    let info = neuros_core::inspect(vfs.as_ref(), probe);
    Ok((src, vfs, info))
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Disks => {
            let disks = neuros_core::rawdisk::list_disks();
            if disks.is_empty() {
                println!("no unmounted disks found (you may need root or the 'disk' group)");
            }
            for d in disks {
                match (&d.probe, &d.error) {
                    (Some(p), _) => println!("{}  {}\n    {}", d.path.display(), d.description, p.summary()),
                    (None, Some(e)) => println!("{}  {}\n    {e}", d.path.display(), d.description),
                    _ => {}
                }
            }
        }
        Cmd::Info(t) => print_info(&open(&t, false)?.2),
        Cmd::Scan(a) => {
            let (src, vfs, mut info) = open(&a.target, false)?;
            let items = do_scan(vfs.as_ref(), &mut info, &a, &src)?;
            print_info(&info);
            for it in &items {
                let t = &it.track;
                println!(
                    "{:<40} | {:<24} | {:<24} | {:>6} | {}",
                    t.title,
                    t.artist.as_deref().unwrap_or(""),
                    t.album.as_deref().unwrap_or(""),
                    fmt_time(t.duration_secs),
                    t.device_path
                );
                for w in &it.warnings {
                    println!("    ! {w}");
                }
            }
            println!("{} files", items.len());
        }
        Cmd::Build { scan, rename, no_backup, backup_dir, keep_the, force, artist_albums, dry_run } => {
            let (src, vfs, mut info) = open(&scan.target, !dry_run)?;
            let items = do_scan(vfs.as_ref(), &mut info, &scan, &src)?;
            print_info(&info);
            if !info.looks_like_neuros() && !force {
                bail!("{} has no sn.txt or WOID_DB; pass --force to write anyway", info.location);
            }
            let opts = WriteOptions {
                build: BuildOptions {
                    ignore_leading_the: !keep_the,
                    artist_albums: match artist_albums {
                        Swap::Auto => info.artist_albums,
                        Swap::On => true,
                        Swap::Off => false,
                    },
                },
                rename_files: rename,
                backup_dir: (!no_backup).then(|| backup_dir.unwrap_or_else(default_backup_dir)),
                drive: None,
                dry_run,
            };
            let r = neuros_core::write_database(vfs.as_ref(), &info, &items, &opts)?;
            vfs.finish().context("flushing to disk")?;
            if let Some(n) = r.artist_album_entries {
                println!("art/alb:   {n} artist entries");
            }
            if dry_run {
                for (from, to) in &r.renamed {
                    println!("would rename: {from} -> {to}");
                }
                println!("dry run: built and verified {} tracks in memory; nothing was written", r.tracks_written);
                return Ok(());
            }
            if let Some(b) = &r.backup {
                println!("backup:    {}", b.display());
            }
            for (from, to) in &r.renamed {
                println!("renamed:   {from} -> {to}");
            }
            for (p, why) in &r.skipped {
                println!("skipped:   {p} ({why}; use --rename)");
            }
            for d in &r.created_dbs {
                println!("created:   empty {d} database");
            }
            println!("wrote {} tracks", r.tracks_written);
        }
        Cmd::Dump(t) => {
            let (_, vfs, _) = open(&t, false)?;
            let db = load_database(vfs.as_ref(), &audio_dir(vfs.as_ref())?, "audio")?;
            for t in db.tracks() {
                println!(
                    "{} | artist={:?} album={:?} genre={:?} rec={:?} | {} | {} KB | {}",
                    t.title,
                    t.keys[1],
                    t.keys[2],
                    t.keys[3],
                    t.keys[4],
                    fmt_time(t.time),
                    t.size_kb,
                    t.path
                );
            }
        }
        Cmd::Add { target, files, to, keep_folders, artist_albums, no_backup } => {
            let (_, vfs, info) = open(&target, true)?;
            print_info(&info);
            let root = vfs::find_dir_ci(vfs.as_ref(), "", "MUSIC").unwrap_or_else(|| "MUSIC".into());
            let layout = match (to, keep_folders) {
                (to, true) => neuros_core::AddLayout::KeepFolders(to.unwrap_or_else(|| root.clone())),
                (Some(f), false) => neuros_core::AddLayout::Folder(f),
                (None, false) => neuros_core::AddLayout::ByTags,
            };
            let plan = neuros_core::plan_add(&files, &layout, &root, true);
            if plan.is_empty() {
                bail!("no MP3 or Ogg files given");
            }
            for p in &plan {
                println!("  {} -> {}", p.host.display(), p.dest);
            }
            let opts = WriteOptions {
                build: BuildOptions {
                    ignore_leading_the: true,
                    artist_albums: match artist_albums {
                        Swap::Auto => info.artist_albums,
                        Swap::On => true,
                        Swap::Off => false,
                    },
                },
                rename_files: true,
                backup_dir: (!no_backup).then(default_backup_dir),
                drive: None,
                dry_run: false,
            };
            let fallback = ScanOptions { music_dir: String::new(), include_recordings: true, placeholders: true };
            let r = neuros_core::add_files(vfs.as_ref(), &info, &plan, &opts, &fallback, |d, t| {
                eprint!("\rcopying {:.1}/{:.1} MB", d as f64 / 1e6, t as f64 / 1e6);
            })?;
            eprintln!();
            vfs.finish().context("flushing to disk")?;
            if let Some(b) = &r.backup {
                println!("backup:    {}", b.display());
            }
            for f in &r.already_present {
                println!("already there: {f}");
            }
            println!("added {} file(s); database has {} tracks", r.added.len(), r.tracks_written);
        }
        Cmd::Sync { target, pc_folder, to, delete, keep_dupes, dry_run, no_backup } => {
            let (_, vfs, info) = open(&target, !dry_run)?;
            print_info(&info);
            let cancel = AtomicBool::new(false);
            let known = info
                .existing
                .as_ref()
                .map(|db| db.tracks().iter().map(|t| t.to_track()).collect::<Vec<_>>())
                .unwrap_or_default();
            let plan = neuros_core::sync::plan_sync(
                vfs.as_ref(),
                &known,
                &pc_folder,
                &to,
                true,
                !keep_dupes,
                &cancel,
                |d, t| {
                    if d % 50 == 0 || d == t {
                        eprint!("\rcomparing {d}/{t}");
                    }
                },
            )?;
            eprintln!();
            use neuros_core::sync::{Match, SyncAction};
            for e in plan.entries.iter().filter(|e| e.action != SyncAction::Unchanged || e.matched != Match::Path) {
                if e.action == SyncAction::Skip {
                    println!(
                        "  {:9} {} = {}",
                        "duplicate",
                        e.host.strip_prefix(&plan.pc_root).unwrap_or(&e.host).display(),
                        e.dup_of.as_deref().unwrap_or("")
                    );
                    continue;
                }
                let a = match (e.action, e.matched) {
                    (SyncAction::Copy, _) => "new",
                    (_, Match::Tags) => "re-tagged",
                    (SyncAction::Update, _) => "changed",
                    _ => "same",
                };
                if e.matched == Match::Path || e.matched == Match::None {
                    println!("  {a:9} {}", e.dest);
                } else {
                    let rel = e.host.strip_prefix(&plan.pc_root).unwrap_or(&e.host);
                    println!("  {a:9} {} = {}", e.dest, rel.display());
                }
            }
            for (p, _) in &plan.extra {
                println!("  {:9} {p}", if delete { "remove" } else { "extra" });
            }
            let (n, c, u) =
                (plan.count(SyncAction::Copy).0, plan.count(SyncAction::Update).0, plan.count(SyncAction::Unchanged).0);
            println!(
                "{n} new, {c} changed, {u} unchanged ({} under another name, {} matched by tags), {} skipped as duplicates, {} only on the Neuros",
                plan.matched(Match::Content),
                plan.matched(Match::Tags),
                plan.count(SyncAction::Skip).0,
                plan.extra.len()
            );
            if dry_run {
                println!("dry run: nothing written");
                return Ok(());
            }
            let opts = WriteOptions {
                build: BuildOptions { ignore_leading_the: true, artist_albums: info.artist_albums },
                rename_files: true,
                backup_dir: (!no_backup).then(default_backup_dir),
                drive: None,
                dry_run: false,
            };
            let fallback = ScanOptions { music_dir: String::new(), include_recordings: true, placeholders: true };
            let r = neuros_core::sync::apply_sync(vfs.as_ref(), &info, &plan, delete, &opts, &fallback, |d, t| {
                eprint!("\rcopying {:.1}/{:.1} MB", d as f64 / 1e6, t as f64 / 1e6);
            })?;
            eprintln!();
            vfs.finish().context("flushing to disk")?;
            println!("copied {}, removed {}; database has {} tracks", r.added.len(), r.deleted.len(), r.tracks_written);
        }
        Cmd::FixText { target, dry_run, no_backup } => {
            let (_, vfs, info) = open(&target, !dry_run)?;
            print_info(&info);
            let opts = WriteOptions {
                build: BuildOptions { ignore_leading_the: true, artist_albums: info.artist_albums },
                rename_files: true,
                backup_dir: (!no_backup).then(default_backup_dir),
                drive: None,
                dry_run,
            };
            let (changes, r) = neuros_core::textfix::fix_database(vfs.as_ref(), &info, &opts)?;
            for c in &changes {
                println!("  {:6} {:>4}x  {}  ->  {}", c.field.name(), c.tracks, c.old, c.new);
            }
            let tracks: usize = changes.iter().map(|c| c.tracks).sum();
            println!("{} values fixed ({tracks} track fields)", changes.len());
            if changes.is_empty() || dry_run {
                if dry_run {
                    println!("dry run: nothing written");
                }
                return Ok(());
            }
            vfs.finish().context("flushing to disk")?;
            if let Some(b) = &r.backup {
                println!("backup:    {}", b.display());
            }
            println!("database rewritten with {} tracks", r.tracks_written);
        }
        Cmd::Dupes { target, other_albums, delete, dry_run, no_backup } => {
            use neuros_core::dedupe::Relation;
            let writing = delete && !dry_run;
            let (_, vfs, info) = open(&target, writing)?;
            print_info(&info);
            let groups = neuros_core::dedupe::find_duplicates(
                vfs.as_ref(),
                &info,
                other_albums,
                &AtomicBool::new(false),
                |d, t| {
                    eprint!("\rreading {d}/{t}");
                },
            )?;
            eprintln!();
            let mut doomed = Vec::new();
            let mut bytes = 0u64;
            for g in &groups {
                println!("{}", g.name);
                for f in &g.files {
                    let del = delete && f.relation != Relation::Keeper && (f.relation.suggest_delete() || other_albums);
                    let mark = if del { "DEL" } else { "" };
                    let t = f.track.duration_secs;
                    println!(
                        "  {mark:3} {:11} {:>3}:{:02} {:>4} kbps {:>5.1} MB  {}",
                        f.relation.label(),
                        t / 60,
                        t % 60,
                        f.kbps,
                        f.size as f64 / 1e6,
                        f.path
                    );
                    if del {
                        doomed.push(f.path.clone());
                        bytes += f.size;
                    }
                }
            }
            let count = |r: Relation| groups.iter().flat_map(|g| &g.files).filter(|f| f.relation == r).count();
            println!(
                "{} songs with copies: {} identical, {} same album, {} other album",
                groups.len(),
                count(Relation::Identical),
                count(Relation::SameAlbum),
                count(Relation::OtherAlbum)
            );
            if doomed.is_empty() {
                return Ok(());
            }
            println!(
                "{} {} files ({:.1} MB)",
                if dry_run { "dry run: would delete" } else { "deleting" },
                doomed.len(),
                bytes as f64 / 1e6
            );
            if dry_run {
                return Ok(());
            }
            let opts = WriteOptions {
                build: BuildOptions { ignore_leading_the: true, artist_albums: info.artist_albums },
                rename_files: true,
                backup_dir: (!no_backup).then(default_backup_dir),
                drive: None,
                dry_run: false,
            };
            let r = neuros_core::dedupe::delete_tracks(vfs.as_ref(), &info, &doomed, &opts)?;
            vfs.finish().context("flushing to disk")?;
            if let Some(b) = &r.backup {
                println!("backup:    {}", b.display());
            }
            println!("deleted {} files; database has {} tracks", r.deleted.len(), r.tracks_written);
        }
        Cmd::Edit { target, pattern, title, artist, album, genre, forget, dry_run } => {
            use neuros_core::overrides::Field;
            let (_, vfs, info) = open(&target, !dry_run)?;
            let db = info.existing.as_ref().context("no database on the device")?;
            let pat = pattern.to_lowercase();
            let tracks: Vec<_> = db
                .tracks()
                .iter()
                .map(|t| t.to_track())
                .filter(|t| {
                    [Some(&t.title), t.artist.as_ref(), t.album.as_ref(), Some(&t.device_path)]
                        .into_iter()
                        .flatten()
                        .any(|s| s.to_lowercase().contains(&pat))
                })
                .collect();
            for t in &tracks {
                println!(
                    "  {} | {} | {} | {}",
                    t.title,
                    t.artist.as_deref().unwrap_or(""),
                    t.album.as_deref().unwrap_or(""),
                    t.device_path
                );
            }
            println!("{} tracks match", tracks.len());
            let sets: Vec<(Field, String)> =
                [(Field::Title, title), (Field::Artist, artist), (Field::Album, album), (Field::Genre, genre)]
                    .into_iter()
                    .filter_map(|(f, v)| v.map(|v| (f, v)))
                    .collect();
            if dry_run || tracks.is_empty() || (sets.is_empty() && !forget) {
                return Ok(());
            }
            if tracks.len() > 1 && sets.iter().any(|(f, _)| *f == Field::Title) {
                bail!("--title would rename {} tracks; narrow --match to one track", tracks.len());
            }
            let mut ed = info.overrides.clone();
            for t in &tracks {
                if forget {
                    ed.clear(&t.device_path);
                }
                for (f, v) in &sets {
                    ed.set(&t.device_path, *f, v);
                }
            }
            let opts = WriteOptions {
                build: BuildOptions { ignore_leading_the: true, artist_albums: info.artist_albums },
                rename_files: true,
                backup_dir: Some(default_backup_dir()),
                drive: None,
                dry_run: false,
            };
            let r = neuros_core::device::save_edits(vfs.as_ref(), &info, &ed, &opts)?;
            vfs.finish().context("flushing to disk")?;
            println!("saved; {} tracks have stored edits", r.edited_tracks);
        }
        Cmd::Backup { target, dest } => {
            let (_, vfs, info) = open(&target, false)?;
            let db = info.db_dir.as_deref().context("no WOID_DB on the device")?;
            let dest = dest.unwrap_or_else(|| {
                default_backup_dir().join(format!(
                    "{}-{}",
                    info.serial.as_deref().unwrap_or("neuros"),
                    neuros_core::device::timestamp()
                ))
            });
            vfs::copy_to_host(vfs.as_ref(), db, &dest)?;
            println!("copied {db} to {}", dest.display());
        }
        Cmd::Verify(t) => {
            let (_, vfs, _) = open(&t, false)?;
            let db = load_database(vfs.as_ref(), &audio_dir(vfs.as_ref())?, "audio")?;
            let all = neuros_core::verify::verify(&db);
            for p in &all {
                println!("{p}");
            }
            let errors = all.iter().filter(|p| !neuros_core::verify::is_note(p)).count();
            if errors > 0 {
                bail!("{errors} problems");
            }
            println!("OK: {} tracks, no problems ({} notes)", db.tracks().len(), all.len());
        }
    }
    Ok(())
}
