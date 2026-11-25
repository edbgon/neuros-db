# Changelog

## [0.1.0-beta.1] - 2026-11-25

First public beta. Tested on Linux with a Neuros (firmware 2.28) on its own
USB and with its hard disk on a USB-IDE adapter. Windows and macOS builds are
provided but untested on hardware.

**Back up your `WOID_DB` folder before first use.** The tool also backs up the
database before every write, but deleted or overwritten music files can't be
restored.

### Features

- Build the Neuros music database (`WOID_DB`) from the MP3 and Ogg Vorbis files
  on the device: Songs, Artists, Albums, Genres, Recordings, and the
  Albums → Artists menu of firmware 2.14+.
- Works on a mounted Neuros (hard disk or flash) and directly on the Neuros
  hard disk connected through a USB-IDE adapter, whose contents are
  byte-swapped (detected automatically).
- **Add music**: copy files or whole folders (with subfolders), sorted into
  Artist/Album folders, keeping their folder structure, or into one folder.
  The database is updated without a full rescan.
- **Sync from PC**: make a folder on the Neuros match a music folder on the
  computer. Recognises files already on the Neuros under other names (by
  content) and re-tagged files (by artist/album/title), skips duplicate copies
  on the PC, and can remove files that are no longer on the PC.
- **Edit** title, artist, album and genre, for one track or many at once.
  Edits are kept in `WOID_DB` and survive rescans and syncs.
- **Find duplicates**: identical files, two rips of one album, tagged and
  untagged copies; optionally the same song on other albums. Suggests which
  copy to keep.
- **Fix "?" in text**: repairs apostrophes and dashes that old taggers saved
  as "?" (`Don?t` → `Don't`, `80?85` → `80-85`), with a preview.
- Text and file names are made ASCII-safe (non-ASCII file names lock up the
  Neuros).
- Sortable track lists, filter, dry runs and a `verify` command.
- GUI (`neuros-gui`) and command line (`neuros-db`).

### Not yet

- Playlists.
