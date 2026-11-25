# neuros-db

Builds and manages the music database for the Neuros (1st-gen, HD or flash)
digital audio player, a modern replacement for positron and NeurosDBM. The
Neuros only plays files listed in its database (`WOID_DB`), which has to be
made on a computer. This tool scans the device and writes it. It can also add
music, sync from a folder on your computer, edit tags, remove duplicates and
fix broken text.

* `neuros-gui` – graphical app (egui), single binary
* `neuros-db` – command-line version of the same thing
* `neuros-core` – the library both use

> **Beta.** Tested on Linux with a Neuros on firmware 2.28, over its own USB
> and with the hard disk on a USB-IDE adapter. Windows and macOS builds are
> provided but haven't been tried on hardware. **Back up the `WOID_DB` folder
> before first use.** The database is backed up before every write, but
> music files that are deleted or overwritten can't be restored. Please report
> problems in the issue tracker.

## Download

Pre-built binaries for Linux (x86_64), Windows (x86_64) and macOS (Apple
Silicon and Intel) are on the
[Releases](https://github.com/edbgon/neuros-db/releases) page. Unpack and
run `neuros-gui` (or `neuros-db` for the command line).

## Build

Needs only a Rust toolchain (rustup, or your distribution's Rust, 1.88+).

    cargo build --release
    ./target/release/neuros-gui            # auto-detects a mounted Neuros
    ./target/release/neuros-gui /mnt/neuros

Builds the same way on Windows and macOS. macOS mounts the Neuros read-only
(a firmware USB bug), so writing there may not work. Raw-disk access (hard
disk on a USB-IDE adapter) needs read/write permission on the disk device
(`disk` group on Linux, administrator on Windows).

## Adding music

GUI: **➕ Add files…** or **📁 Add folders…** (folders include all their
subfolders; you can also drop files/folders onto the window), choose where
they go, check the preview, press *Copy … and update database*. Where files go:

- *sorted into MUSIC/Artist/Album* from their tags (default for files), or
- *keeping their folders* under a Neuros folder: picking `~/Music/Beck` gives
  `MUSIC/Beck/<album>/…` (default when you pick folders), or
- *all in one folder*.

CLI:

    neuros-db add /dev/sdX ~/Music/NewAlbum          # → MUSIC/Artist/Album/
    neuros-db add /dev/sdX ~/Music/Beck --keep-folders   # → MUSIC/Beck/...
    neuros-db add /mnt/neuros song.mp3 --to MUSIC/Misc

Works the same on a mounted Neuros and on the disk on a USB-IDE adapter.
File names are made ASCII-safe; files already on the device (same name and
size) aren't copied twice. The database is updated from its existing
records plus the new files, so there's no full rescan (album track order is
kept). Files you copied on by other means, or deleted, are only picked up
by a full Scan + Write.

## Sync from the PC

GUI: **⇄ Sync from PC…** → pick the music folder on the computer and the
Neuros folder (default `MUSIC`) → **Compare** → review → **Sync now**. CLI:

    neuros-db sync /dev/sdX ~/Music --dry-run
    neuros-db sync /dev/sdX ~/Music --delete

Each PC file is paired with a file on the Neuros, in this order:

1. **same path** (names made ASCII-safe) — compared by size;
2. **same contents under another name** — for music put on by older tools
   that renamed files (`1 - Artist - Title.mp3` vs `01_Title.mp3`). Same size
   plus the same duration and title in the database; if that's not
   conclusive, 16 KiB from the middle of both files are compared;
3. **same artist, album and title** in the database (only when unique on
   both sides) — a file re-tagged or re-encoded on the PC, copied over the
   Neuros copy in place.

Unpaired PC files are new and go into the Neuros folder mirroring the PC
structure. Paired files keep their Neuros names (nothing is renamed or
moved); unchanged ones aren't copied but their database info is refreshed
from the PC tags. Files only on the Neuros are kept unless you tick "remove
them" (`--delete`). Tracks outside the synced folder (e.g. recordings) are
left alone. The GUI remembers the last PC folder.

New PC files that are second copies of a song already on the Neuros (or
already in this sync) from the same album — e.g. two rips of one album on
the PC — are skipped, so duplicates you removed don't come back. Untick
*Skip PC files that are second copies…* (`--keep-dupes`) to copy them anyway.

## Duplicates

GUI: **🧹 Find duplicates…** lists songs that are on the Neuros more than
once. Tick the copies to delete, then **Delete … and update database**
(asks again; files can't be undeleted). CLI:

    neuros-db dupes /dev/sdX                     # list
    neuros-db dupes /dev/sdX --delete --dry-run  # what would go
    neuros-db dupes /dev/sdX --delete

Each copy is marked by how it relates to the suggested keeper:

- **identical** — same size and audio; ticked;
- **same album** — same artist, title and album (track number from the file
  name must agree too, lengths within 3 s): two rips of one album, or a
  tagged and an untagged copy; ticked;
- **other album** — the same song on a best-of or compilation; only shown
  with *Also show the same song on other albums* (`--other-albums`), never
  ticked for you.

Titles ignore case, punctuation and brackets ("(Remastered)"), but "(live)",
"(remix)", "(demo)", "(instrumental)" etc. are different songs. Two files in
the same folder are never copies of each other unless identical. Untagged
files get artist and title from the file name and folders. The suggested
keeper has real tags, then a real album, then the higher bitrate.
Recordings are left out. A group can't lose all its copies.

## Fixing "?" in names

Taggers that saved tags as Latin-1 turned curly apostrophes and dashes into
"?": `Don?t`, `80?85`, `Heart?Shaped Box`. Tags are repaired as they are
read (scan, add, sync), and **✒ Fix "?" in text…** (CLI: `neuros-db
fix-text <target> [--dry-run]`) repairs the database on the device, after a
preview. Fixed: contractions and dropped g's (`Don't`, `Bringin'`), elisions
(`'em`, `Rock 'n' Roll`, `'78`, `O'Brother`, `d'ici`), single-quoted words
(`'B' Sides`), number ranges (`1974-1978`), compounds (`Heart-Shaped`,
`La-Z-Boy`, `I-D-I-O-T`) and ` ? ` separators. Real question marks, `??`
runs (other scripts) and unclear cases (a lost "…") stay. The music files
themselves aren't changed.

## Editing

The **On Neuros** view lists the database on the device. Select tracks
(Ctrl/Shift-click for several), change Title / Artist / Album / Genre in the
panel on the right, **Apply**, then **Save edits to Neuros**. CLI:

    neuros-db edit /dev/sdX --match "abba" --artist "ABBA"

Edits change the database, not the music files. They're kept on the device
in `WOID_DB/neuros-db-edits.tsv` and re-applied on every rescan, add and
sync, so they don't get lost. "Forget edits" removes them; the file tags
come back with the next Scan + Write.

## Hard disk on a USB-IDE adapter

The Neuros stores its disk byte-swapped (every 16-bit word has its bytes the
other way round, like `dd conv=swab`), so when the drive is connected to a
PC directly the OS can't mount it. neuros-db reads and writes such disks
itself: a byte-swapping block layer plus a built-in FAT32 driver (fatfs), so
nothing needs mounting and it works the same on Linux, Windows and macOS.

* GUI: choose "Hard disk on USB-IDE adapter / disk image"; swapped disks are
  marked "✔ Neuros". Disk images (`.img`) work too.
* CLI: pass the device instead of a folder: `neuros-db info /dev/sdX`.
  `neuros-db disks` lists candidates. Byte order is auto-detected from the
  boot signature (`AA 55` instead of `55 AA`); override with `--swap on|off`.
* Raw access needs permission on the device node: root, or on Linux
  membership of the `disk` group. Disks the OS has mounted are never
  written.

## CLI

    neuros-db info   /mnt/neuros          # serial, firmware, drive letter, current DB
    neuros-db scan   /mnt/neuros          # list what would be written + warnings
    neuros-db build  /mnt/neuros --rename # back up, rename bad names, write DB
    neuros-db build  /dev/sdX --dry-run   # build + verify in memory, write nothing
    neuros-db add    /dev/sdX FILES…      # copy music on + update the database
    neuros-db sync   /dev/sdX ~/Music     # mirror a PC folder onto the Neuros
    neuros-db dupes  /dev/sdX             # find duplicate songs (--delete)
    neuros-db fix-text /dev/sdX --dry-run # repair "?" for lost ' and -
    neuros-db edit   /dev/sdX --match X --artist Y   # edit database fields
    neuros-db dump   /mnt/neuros          # print the tracks in the current DB
    neuros-db verify /mnt/neuros          # consistency check of the current DB
    neuros-db backup /mnt/neuros [DIR]    # copy WOID_DB to this computer
    neuros-db disks                       # list raw disks (USB-IDE adapter)

Options: `--music DIR` (scan only part of the device), `--drive C|D`,
`--no-recordings`, `--no-placeholders`, `--keep-the`, `--no-backup`,
`--backup-dir DIR`, `--force`, `--artist-albums auto|on|off`, `--dry-run`.

## What it does

1. Scans the chosen folder (default: the whole device, skipping `WOID_*`
   system folders) for `.mp3` / `.ogg`, reading tags and duration.
   Optionally includes `WOID_RECORD` recordings, keeping their
   FM Radio / Microphone type from the existing database.
2. Transliterates metadata to ASCII (the Neuros can't display anything else).
   File names with non-ASCII characters lock the player up, so they are
   renamed (`--rename` / GUI checkbox) or left out.
3. Builds the whole audio database in memory from known-good empty templates
   (from positron), sorted the way the menus should show it: songs by title,
   artists/albums/genres alphabetically ("The" ignored), album tracks by
   disc/track number.
4. Parses back and verifies everything it built, copies the current `WOID_DB`
   to `~/.local/share/neuros-db/backups/<serial>-<time>/`, writes
   `WOID_DB/audio.new/` and swaps it in. Missing pcaudio/HiSi databases
   are created empty.

Paths are stored as `C:/…` (hard-disk backpack) or `D:/…` (flash). The letter
is taken from the existing database if one exists, otherwise from the
filesystem size (< 2 GB = flash), and can be overridden.

### Artist → Album menu (firmware 2.14+)

With `--artist-albums on` (GUI: "Artist → Album menu"), the database is
written in NeurosDBM's layout: `albums.mdb` gets "All" / "Artists"
sub-menus and `artistalbum.*` lists, per artist, the albums whose tracks
all have that artist. Albums with several artists go under
"Various Artists" (first in the list); albums with no artist tag aren't
listed there. Default (`auto`): on when the current database already has
it or `version.txt` says firmware ≥ 2.14, otherwise off.

Checked against a real NeurosDBM database (fw 2.28, 6628 tracks): 221 of
228 artists identical; the rest are case-only duplicates NeurosDBM kept
apart ("Drums And Wires" / "Drums and Wires", merged here) and entries its
data had let go stale.

Not supported yet: playlists, incremental updates.

## Format notes

See positron's `doc/database.html` and friends. Things that matter here:
16-bit big-endian words, pointers count words, `#`/`$`/`%`/`/` special
words escaped inside fields, a null record first in every MDB with its own
SAI entry, PAI modules in multiples of 32 words, SAI PAI-pointers to the
first entry (module start + 6), and the isModified status bit must be set or
firmware ≥ 1.45 ignores the database. The MDB headers (including the
undocumented XIM menu block) are copied verbatim from the templates; an
empty build is byte-identical to positron's templates (see `tests/build.rs`).

## Testing on hardware

1. `neuros-db info` and `neuros-db verify` on the device as-is (read-only).
2. Back up `WOID_DB` by hand too, the first time.
3. Build with one album, eject, check Songs / Artists / Albums / Genres.
4. Then the whole library.

## License

MIT, see [LICENSE](LICENSE). The database format work is based on
[positron](https://www.xiph.org/positron/) (BSD, Xiph.org Foundation); see
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
