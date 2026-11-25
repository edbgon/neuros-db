//! User edits to database fields, kept on the device in
//! `WOID_DB/neuros-db-edits.tsv` so they survive every rebuild (full
//! rescans, adds, syncs).  The firmware ignores unknown files in WOID_DB.
//!
//! Format: one line per edited field, `device path <TAB> field <TAB> value`.
//! An empty value clears the field (artist/album/genre).

use std::collections::BTreeMap;

use crate::Track;
use crate::scan::sanitize_text;
use crate::vfs::{self, Vfs};

pub const EDITS_FILE: &str = "neuros-db-edits.tsv";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Field {
    Title,
    Artist,
    Album,
    Genre,
}

impl Field {
    pub const ALL: [Field; 4] = [Field::Title, Field::Artist, Field::Album, Field::Genre];

    pub fn name(self) -> &'static str {
        match self {
            Field::Title => "title",
            Field::Artist => "artist",
            Field::Album => "album",
            Field::Genre => "genre",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.name() == s)
    }

    /// Current value of this field on a track.
    pub fn get(self, t: &Track) -> Option<&str> {
        match self {
            Field::Title => Some(&t.title),
            Field::Artist => t.artist.as_deref(),
            Field::Album => t.album.as_deref(),
            Field::Genre => t.genre.as_deref(),
        }
    }
}

/// Per-track edits, keyed by lower-cased device path (`C:/MUSIC/...`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    map: BTreeMap<String, BTreeMap<Field, String>>,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Number of edited tracks.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn parse(text: &str) -> Self {
        let mut o = Self::default();
        for line in text.lines() {
            let mut parts = line.splitn(3, '\t');
            if let (Some(p), Some(f), Some(v)) = (parts.next(), parts.next(), parts.next())
                && let Some(f) = Field::parse(f)
            {
                o.map.entry(p.to_lowercase()).or_default().insert(f, v.trim_end_matches('\r').to_string());
            }
        }
        o
    }

    pub fn to_text(&self) -> String {
        let mut s = String::new();
        for (p, fields) in &self.map {
            for (f, v) in fields {
                s.push_str(&format!("{p}\t{}\t{v}\r\n", f.name()));
            }
        }
        s
    }

    /// Read the edits file from the device (missing file = no edits).
    pub fn load(vfs: &dyn Vfs, db_dir: Option<&str>) -> Self {
        db_dir
            .and_then(|d| vfs::find_file_ci(vfs, d, EDITS_FILE))
            .and_then(|p| vfs.read_file(&p).ok())
            .map(|b| Self::parse(&String::from_utf8_lossy(&b)))
            .unwrap_or_default()
    }

    /// Set a field for one track (made ASCII-safe; empty clears
    /// artist/album/genre; an empty title is ignored).
    pub fn set(&mut self, device_path: &str, field: Field, value: &str) {
        let v = sanitize_text(value).replace('\t', " ");
        if field == Field::Title && v.is_empty() {
            return; // a track needs a title
        }
        self.map.entry(device_path.to_lowercase()).or_default().insert(field, v);
    }

    /// Forget all edits of one track, so its file tags apply again on the
    /// next full rescan.
    pub fn clear(&mut self, device_path: &str) {
        self.map.remove(&device_path.to_lowercase());
    }

    pub fn get(&self, device_path: &str, field: Field) -> Option<&str> {
        self.map.get(&device_path.to_lowercase())?.get(&field).map(String::as_str)
    }

    pub fn is_edited(&self, device_path: &str) -> bool {
        self.map.contains_key(&device_path.to_lowercase())
    }

    /// Layer `other` on top of `self`.
    pub fn merge(&mut self, other: &Overrides) {
        for (p, fields) in &other.map {
            let e = self.map.entry(p.clone()).or_default();
            for (f, v) in fields {
                e.insert(*f, v.clone());
            }
        }
    }

    /// Apply edits to a track.
    pub fn apply(&self, t: &mut Track) {
        let Some(fields) = self.map.get(&t.device_path.to_lowercase()) else { return };
        for (f, v) in fields {
            let val = (!v.is_empty()).then(|| v.clone());
            match f {
                Field::Title => {
                    if let Some(v) = val {
                        t.title = v;
                    }
                }
                Field::Artist => t.artist = val,
                Field::Album => t.album = val,
                Field::Genre => t.genre = val,
            }
        }
    }

    /// Drop edits for tracks that are no longer in the database.
    pub fn pruned(&self, tracks: &[Track]) -> Self {
        let live: std::collections::HashSet<String> = tracks.iter().map(|t| t.device_path.to_lowercase()).collect();
        Self { map: self.map.iter().filter(|(p, _)| live.contains(*p)).map(|(p, f)| (p.clone(), f.clone())).collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> Track {
        Track {
            title: "Old".into(),
            artist: Some("A".into()),
            album: Some("B".into()),
            genre: None,
            recording: None,
            disc: None,
            track_no: None,
            duration_secs: 1,
            size_kb: 1,
            device_path: "C:/MUSIC/x.mp3".into(),
        }
    }

    #[test]
    fn roundtrip_and_apply() {
        let mut o = Overrides::default();
        o.set("C:/MUSIC/X.mp3", Field::Title, "Néw");
        o.set("C:/MUSIC/X.mp3", Field::Album, "");
        o.set("C:/MUSIC/X.mp3", Field::Title, ""); // ignored
        let o2 = Overrides::parse(&o.to_text());
        assert_eq!(o, o2);
        let mut t = track();
        o2.apply(&mut t);
        assert_eq!(t.title, "New");
        assert_eq!(t.album, None);
        assert_eq!(t.artist.as_deref(), Some("A"));
        let mut o3 = o2.clone();
        o3.clear("c:/music/x.mp3");
        assert!(o3.is_empty());
    }
}
