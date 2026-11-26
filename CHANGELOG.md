# Changelog

## [Unreleased]

- Delete tracks from the Neuros: select them, then *🗑 Delete from Neuros…*
  or the Delete key. CLI: `neuros-db delete --match X --yes`.
- Clicking a track's text now selects the row.
- Sync no longer copies a file over one of another type with the same tags
  (e.g. an MP3 converted from an Ogg replacing the old `.ogg` in place).

## [0.1.0-beta.2] - 2026-11-26

- The window opens right away; the device and its database are read in the
  background with a progress indicator (slow over the Neuros's USB 1.1).
- Tidier main window: fixed toolbar, settings moved to an Options window,
  status bar.
- App icon, also on Wayland: on Linux the icon and a hidden `.desktop` entry
  are installed for the user at startup. *Options → Add to the application
  menu* adds it to the menu.

## [0.1.0-beta.1] - 2026-11-25

First public beta. Tested on Linux with firmware 2.28, over the Neuros's own
USB and with the hard disk on a USB-IDE adapter. Windows and macOS builds are
untested on hardware. Back up `WOID_DB` before first use.

- Build the database from the MP3 and Ogg files on the Neuros.
- Add files or folders; sync a music folder from the computer.
- Edit title, artist, album and genre.
- Find and remove duplicate songs.
- Repair `?` left by old taggers.
- Albums → Artists menu (firmware 2.14+).
- Hard disk on a USB-IDE adapter (byte-swapped) supported.

Not yet: playlists.
