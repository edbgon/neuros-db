# neuros-db

Music database builder for the 1st-generation Neuros audio player (hard disk
or flash). The Neuros only plays files listed in its database (`WOID_DB`),
which has to be made on a computer. A replacement for positron and NeurosDBM.

> **Beta.** Tested on Linux with firmware 2.28. Windows and macOS builds are
> untested on hardware. Back up `WOID_DB` before first use.

## Download

Linux, Windows and macOS builds are on the
[Releases](https://github.com/edbgon/neuros-db/releases) page. Run
`neuros-gui`, or `neuros-db` for the command line.

On Linux, *Options → Add to the application menu* installs a menu entry and
the icon.

## Features

- Build the database from the MP3 and Ogg files on the Neuros.
- Add files or folders from the computer.
- Sync a music folder from the computer. Files already on the Neuros under
  other names are recognised and not copied again.
- Edit title, artist, album and genre. Edits survive rebuilds.
- Find and remove duplicate songs.
- Repair `?` left by old taggers (`Don?t` → `Don't`).
- Albums → Artists menu (firmware 2.14+).
- Works with the hard disk on a USB-IDE adapter (see below).

The database is backed up before every write, to
`~/.local/share/neuros-db/backups`. Music files that are deleted or
overwritten can't be restored.

## Hard disk on a USB-IDE adapter

The Neuros stores its disk byte-swapped, so a computer can't mount it when
the drive is connected directly. neuros-db reads and writes it anyway:
choose *Hard disk on USB-IDE adapter* in the GUI, or give the CLI the device
(`neuros-db info /dev/sdX`). This needs read/write access to the device
(`disk` group on Linux, administrator on Windows).

## Command line

    neuros-db info     <target>               # device and database summary
    neuros-db scan     <target>               # what a rebuild would write
    neuros-db build    <target> [--dry-run]   # rebuild the database
    neuros-db add      <target> FILES|DIRS    # copy music on
    neuros-db sync     <target> ~/Music [--delete] [--dry-run]
    neuros-db edit     <target> --match X --artist Y
    neuros-db dupes    <target> [--delete] [--dry-run]
    neuros-db fix-text <target> [--dry-run]
    neuros-db verify   <target>
    neuros-db backup   <target> [DIR]
    neuros-db disks                           # disks on a USB-IDE adapter

`<target>` is the mounted Neuros (`/run/media/you/NEUROS`) or a raw disk
(`/dev/sdX`). `neuros-db <command> --help` lists the options.

## Build

Rust 1.88 or newer:

    cargo build --release

## License

MIT. The format work is based on [positron](https://www.xiph.org/positron/)
(BSD, Xiph.org Foundation); see [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
