//! Builds a complete `WOID_DB/audio` database from a list of tracks.
//!
//! We always rebuild from scratch (the positron docs recommend this over
//! in-place edits).  Headers, including the undocumented XIM menu block,
//! are copied verbatim from known-good empty templates.

use std::collections::HashMap;

use crate::templates;
use crate::words::*;
use crate::{Error, Result, Track};

/// Access keys of the audio database, in header order.
pub const AUDIO_CHILDREN: [&str; 5] = ["playlist", "artist", "albums", "genre", "recordings"];

#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Sort "The Examples" under E.
    pub ignore_leading_the: bool,
    /// Write the firmware >= 2.14 Artist/Album sub-database
    /// (`artistalbum.*`, Albums → Artists menu) using NeurosDBM's layout.
    pub artist_albums: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self { ignore_leading_the: true, artist_albums: false }
    }
}

/// Name of the artist entry that collects albums with several artists.
pub const VARIOUS_ARTISTS: &str = "Various Artists";

/// One output file: name relative to the database directory, and contents.
#[derive(Debug, Clone)]
pub struct DbFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Take the header (first LengthOfHeader words) of a template MDB and set
/// the isModified status bit, which firmware >= 1.45 requires.
fn template_header(tpl: &[u8]) -> Vec<u8> {
    let len = word_at(tpl, 0).expect("template header") as usize * 2;
    let mut h = tpl[..len].to_vec();
    h[5] |= 0x01; // Status word (word 2), bit 0
    h
}

fn template_header_words(tpl: &[u8]) -> u32 {
    word_at(tpl, 0).expect("template header") as u32
}

fn sai_bytes(entries: &[(u32, u32)]) -> Vec<u8> {
    let mut b = WordBuf::new();
    b.u32(SAI_SIGNATURE);
    b.u32(0);
    b.word(entries.len() as u16);
    for _ in 0..3 {
        b.word(0);
    }
    b.u32(0);
    b.u32(0);
    for &(mdb, pai) in entries {
        b.u32(mdb);
        b.u32(pai);
    }
    b.u32(0);
    b.u32(0);
    b.into_bytes()
}

/// Append a PAI module; returns the pointer SAI uses (first entry).
fn push_pai_module(b: &mut WordBuf, entries: &[u32]) -> u32 {
    let n = entries.len() as u32;
    // header (6) + entries + at least a 2-word zero terminator, rounded to 32
    let len = (6 + 2 * n + 2).div_ceil(32) * 32;
    let start = b.len();
    b.word(len as u16);
    b.word(if n == 0 { 0x0001 } else { 0x0000 });
    b.word(n as u16);
    for _ in 0..3 {
        b.word(0);
    }
    for &e in entries {
        b.u32(e);
    }
    while b.len() < start + len {
        b.word(0);
    }
    start + 6
}

pub fn sort_key(s: &str, ignore_the: bool) -> String {
    let l = s.trim().to_lowercase();
    if ignore_the && let Some(rest) = l.strip_prefix("the ") {
        return rest.trim_start().to_string();
    }
    l
}

/// Distinct values of one access key, grouped case-insensitively.
struct KeyIndex {
    /// Display value per group, in final (sorted) order.
    values: Vec<String>,
    /// Track index → group index (None = null record).
    per_track: Vec<Option<usize>>,
}

fn index_key(tracks: &[Track], get: impl Fn(&Track) -> Option<&str>, ignore_the: bool) -> KeyIndex {
    let mut first_seen: Vec<String> = Vec::new();
    let mut lookup: HashMap<String, usize> = HashMap::new();
    let mut raw: Vec<Option<usize>> = Vec::with_capacity(tracks.len());
    for t in tracks {
        let v = get(t).map(str::trim).filter(|s| !s.is_empty());
        raw.push(v.map(|v| {
            *lookup.entry(v.to_lowercase()).or_insert_with(|| {
                first_seen.push(v.to_string());
                first_seen.len() - 1
            })
        }));
    }
    let mut order: Vec<usize> = (0..first_seen.len()).collect();
    order.sort_by_cached_key(|&i| (sort_key(&first_seen[i], ignore_the), first_seen[i].clone()));
    let mut remap = vec![0; order.len()];
    for (new, &old) in order.iter().enumerate() {
        remap[old] = new;
    }
    KeyIndex {
        values: order.iter().map(|&i| first_seen[i].clone()).collect(),
        per_track: raw.into_iter().map(|g| g.map(|g| remap[g])).collect(),
    }
}

fn opt_lower(s: &Option<String>) -> String {
    s.as_deref().unwrap_or("").to_lowercase()
}

/// Build all files of the `audio` database.  `tracks` need not be sorted.
pub fn build_audio(tracks: &[Track], opts: &BuildOptions) -> Result<Vec<DbFile>> {
    // Artist/Album mode uses NeurosDBM's headers for audio.mdb and
    // albums.mdb; everything else is shared.
    let tpl = |name: &str| -> &'static [u8] {
        opts.artist_albums
            .then(|| templates::get(&format!("audio-aa/{name}")))
            .flatten()
            .or_else(|| templates::get(&format!("audio/{name}")))
            .expect("template")
    };
    let tpl_root = tpl("audio.mdb");
    {
        let parsed = crate::reader::parse_mdb(tpl_root, "template audio.mdb")?;
        let files: Vec<_> = parsed.rules.iter().skip(1).filter_map(|r| r.filename.clone()).collect();
        let expected: Vec<_> = AUDIO_CHILDREN.iter().map(|c| format!("{c}.mdb")).collect();
        if files != expected || parsed.num_fields != 9 {
            return Err(Error::Corrupt("audio template has unexpected layout".into()));
        }
    }

    // Final track order = Songs menu order: by title, then path.
    let mut order: Vec<usize> = (0..tracks.len()).collect();
    order.sort_by_cached_key(|&i| (tracks[i].title.to_lowercase(), tracks[i].device_path.to_lowercase()));
    let tracks: Vec<&Track> = order.iter().map(|&i| &tracks[i]).collect();
    let owned: Vec<Track> = tracks.iter().map(|t| (*t).clone()).collect();

    let the = opts.ignore_leading_the;
    let keys: [KeyIndex; 5] = [
        index_key(&owned, |_| None, the), // playlists: not supported yet
        index_key(&owned, |t| t.artist.as_deref(), the),
        index_key(&owned, |t| t.album.as_deref(), the),
        index_key(&owned, |t| t.genre.as_deref(), the),
        index_key(&owned, |t| t.recording.as_deref(), false),
    ];

    // --- Child MDBs: null record first, then the sorted values.
    let mut out = Vec::new();
    // child_ptrs[k][group] and child_null[k]
    let mut child_ptrs: Vec<Vec<u32>> = Vec::new();
    let mut child_null: Vec<u32> = Vec::new();
    let mut child_mdb_bytes: Vec<Vec<u8>> = Vec::new();
    for (k, name) in AUDIO_CHILDREN.iter().enumerate() {
        let t = tpl(&format!("{name}.mdb"));
        let mut b = WordBuf::new();
        b.raw_bytes(&template_header(t));
        debug_assert_eq!(b.len(), template_header_words(t));
        child_null.push(b.len());
        b.word(RECORD_FLAG);
        b.word(END_OF_RECORD);
        let mut ptrs = Vec::new();
        for v in &keys[k].values {
            ptrs.push(b.len());
            b.word(RECORD_FLAG);
            b.escaped_sz(v);
            b.word(END_OF_RECORD);
        }
        child_ptrs.push(ptrs);
        child_mdb_bytes.push(b.into_bytes());
    }

    // --- Root audio MDB.
    let mut b = WordBuf::new();
    b.raw_bytes(&template_header(tpl_root));
    let root_null = b.len();
    b.word(RECORD_FLAG);
    b.word(END_OF_RECORD);
    let mut track_ptrs = Vec::with_capacity(tracks.len());
    for (ti, t) in tracks.iter().enumerate() {
        track_ptrs.push(b.len());
        b.word(RECORD_FLAG);
        b.escaped_sz(&t.title);
        for k in 0..5 {
            b.word(FIELD_DELIM);
            let p = match keys[k].per_track[ti] {
                Some(g) => child_ptrs[k][g],
                None => child_null[k],
            };
            b.escaped_u32(p);
        }
        b.word(FIELD_DELIM);
        b.escaped_u32(t.duration_secs);
        b.word(FIELD_DELIM);
        b.escaped_u32(t.size_kb);
        b.word(FIELD_DELIM);
        b.escaped_sz(&t.device_path);
        b.word(END_OF_RECORD);
    }
    out.push(DbFile { name: "audio.mdb".into(), bytes: b.into_bytes() });
    let mut root_sai = vec![(root_null, 0)];
    root_sai.extend(track_ptrs.iter().map(|&p| (p, 0)));
    out.push(DbFile { name: "audio.sai".into(), bytes: sai_bytes(&root_sai) });

    // --- Child PAI + SAI.  Back-pointer order inside each module decides
    // the order tracks are listed under that artist/album/genre.
    let album_order = |&ti: &usize| {
        let t = tracks[ti];
        (t.disc.unwrap_or(0), t.track_no.unwrap_or(u32::MAX), t.title.to_lowercase(), ti)
    };
    for (k, name) in AUDIO_CHILDREN.iter().enumerate() {
        let n_groups = keys[k].values.len();
        let mut members: Vec<Vec<usize>> = vec![Vec::new(); n_groups];
        let mut null_members: Vec<usize> = Vec::new();
        for (ti, g) in keys[k].per_track.iter().enumerate() {
            match g {
                Some(g) => members[*g].push(ti),
                None => null_members.push(ti),
            }
        }
        let sort = |list: &mut Vec<usize>| match *name {
            "albums" => list.sort_by_cached_key(album_order),
            "artist" => list.sort_by_cached_key(|&ti| {
                let t = tracks[ti];
                (sort_key(t.album.as_deref().unwrap_or(""), the), album_order(&ti))
            }),
            "genre" => list.sort_by_cached_key(|&ti| {
                let t = tracks[ti];
                (sort_key(t.artist.as_deref().unwrap_or(""), the), opt_lower(&t.album), album_order(&ti))
            }),
            _ => {} // already in title order
        };

        let mut pai = WordBuf::new();
        pai.raw_bytes(&tpl(&format!("{name}.pai"))[..16]);
        let mut sai = Vec::with_capacity(n_groups + 1);

        // Null record: module only if something refers to it.
        if null_members.is_empty() {
            sai.push((child_null[k], 0));
        } else {
            sort(&mut null_members);
            let e: Vec<u32> = null_members.iter().map(|&ti| track_ptrs[ti]).collect();
            sai.push((child_null[k], push_pai_module(&mut pai, &e)));
        }
        for g in 0..n_groups {
            sort(&mut members[g]);
            let e: Vec<u32> = members[g].iter().map(|&ti| track_ptrs[ti]).collect();
            sai.push((child_ptrs[k][g], push_pai_module(&mut pai, &e)));
        }
        out.push(DbFile { name: format!("{name}.mdb"), bytes: std::mem::take(&mut child_mdb_bytes[k]) });
        out.push(DbFile { name: format!("{name}.sai"), bytes: sai_bytes(&sai) });
        out.push(DbFile { name: format!("{name}.pai"), bytes: pai.into_bytes() });
    }

    if opts.artist_albums {
        out.extend(build_artist_albums(&keys[1], &keys[2], &child_ptrs[2], tpl("artistalbum.mdb"), tpl("albums.pai")));
    }
    Ok(out)
}

/// The Artist → Album sub-database (child of albums.mdb), as NeurosDBM
/// writes it: an album belongs to an artist if all its tracks have that
/// artist; albums with several artists go under "Various Artists" (listed
/// first).  Each record's PAI module points at album records.
fn build_artist_albums(
    artists: &KeyIndex,
    albums: &KeyIndex,
    album_ptrs: &[u32],
    tpl_mdb: &[u8],
    tpl_pai: &[u8],
) -> Vec<DbFile> {
    #[derive(Clone, Copy, PartialEq)]
    enum Owner {
        Unset,
        One(Option<usize>),
        Mixed,
    }
    let mut owner = vec![Owner::Unset; albums.values.len()];
    for (ti, alb) in albums.per_track.iter().enumerate() {
        let Some(g) = *alb else { continue };
        let a = artists.per_track[ti];
        owner[g] = match owner[g] {
            Owner::Unset => Owner::One(a),
            Owner::One(x) if x == a => Owner::One(x),
            _ => Owner::Mixed,
        };
    }
    // An artist literally called "Various Artists" merges into the compilation entry.
    let va_artist = artists.values.iter().position(|v| v.eq_ignore_ascii_case(VARIOUS_ARTISTS));
    let mut compilations = Vec::new();
    let mut by_artist: Vec<Vec<usize>> = vec![Vec::new(); artists.values.len()];
    for (g, o) in owner.iter().enumerate() {
        match *o {
            Owner::Mixed => compilations.push(g),
            Owner::One(Some(a)) if Some(a) == va_artist => compilations.push(g),
            Owner::One(Some(a)) => by_artist[a].push(g),
            _ => {} // only untagged tracks: not listed, like NeurosDBM
        }
    }

    let mut entries: Vec<(&str, &[usize])> = Vec::new();
    if !compilations.is_empty() {
        entries.push((VARIOUS_ARTISTS, &compilations));
    }
    for (a, list) in by_artist.iter().enumerate() {
        if !list.is_empty() {
            entries.push((&artists.values[a], list));
        }
    }

    let mut mdb = WordBuf::new();
    mdb.raw_bytes(&template_header(tpl_mdb));
    let null = mdb.len();
    mdb.word(RECORD_FLAG);
    mdb.word(END_OF_RECORD);
    let mut pai = WordBuf::new();
    pai.raw_bytes(&tpl_pai[..16]);
    let mut sai = vec![(null, 0)];
    for (name, list) in entries {
        let ptr = mdb.len();
        mdb.word(RECORD_FLAG);
        mdb.escaped_sz(name);
        mdb.word(END_OF_RECORD);
        let e: Vec<u32> = list.iter().map(|&g| album_ptrs[g]).collect();
        sai.push((ptr, push_pai_module(&mut pai, &e)));
    }
    vec![
        DbFile { name: "artistalbum.mdb".into(), bytes: mdb.into_bytes() },
        DbFile { name: "artistalbum.sai".into(), bytes: sai_bytes(&sai) },
        DbFile { name: "artistalbum.pai".into(), bytes: pai.into_bytes() },
    ]
}
