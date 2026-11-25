//! Parsers for MDB, SAI and PAI files.  Used to verify what we write,
//! dump databases, and pick up information from an existing device DB.

use crate::vfs::Vfs;
use std::collections::BTreeMap;

use crate::words::*;
use crate::{Error, Result};

/// One MDB record.  `fields[field][value]` is the unescaped words of one
/// value.  The null record has no fields.
#[derive(Debug, Clone)]
pub struct MdbRecord {
    pub ptr: u32,
    pub flags: u16,
    /// Length of the record in words, including flag and terminator.
    pub len: u32,
    pub fields: Vec<Vec<Vec<u16>>>,
}

impl MdbRecord {
    pub fn is_null(&self) -> bool {
        self.fields.is_empty()
    }
    pub fn is_deleted(&self) -> bool {
        self.flags & RECORD_DELETED != 0
    }
    /// Primary field as a string.
    pub fn primary(&self) -> Option<String> {
        self.fields.first()?.first().map(|v| decode_sz(v))
    }
    /// Values of a pointer field (access key).
    pub fn pointers(&self, field: usize) -> Vec<u32> {
        self.fields
            .get(field)
            .map(|vals| vals.iter().filter(|v| v.len() == 2).map(|v| ((v[0] as u32) << 16) | v[1] as u32).collect())
            .unwrap_or_default()
    }
    pub fn u32_field(&self, field: usize) -> Option<u32> {
        self.pointers(field).first().copied()
    }
    pub fn sz_field(&self, field: usize) -> Option<String> {
        self.fields.get(field)?.first().map(|v| decode_sz(v))
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub filename: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Mdb {
    pub header_len: u32,
    pub attributes: u16,
    pub status: u16,
    pub num_keys: u16,
    pub num_fields: u16,
    pub record_start: u32,
    pub db_name: String,
    pub rules: Vec<Rule>,
    /// Records keyed by word pointer.
    pub records: BTreeMap<u32, MdbRecord>,
    pub file_words: u32,
}

fn dd_at(bytes: &[u8], ptr: u32) -> Option<String> {
    let len = word_at(bytes, ptr)? as u32;
    let words: Vec<u16> = (0..len).map(|i| word_at(bytes, ptr + 1 + i)).collect::<Option<_>>()?;
    Some(decode_sz(&words))
}

fn sz_at(bytes: &[u8], ptr: u32) -> Option<String> {
    let mut words = Vec::new();
    let mut p = ptr;
    loop {
        let w = word_at(bytes, p)?;
        words.push(w);
        if w & 0x00ff == 0 {
            break;
        }
        p += 1;
    }
    Some(decode_sz(&words))
}

pub fn parse_mdb(bytes: &[u8], name: &str) -> Result<Mdb> {
    let bad = |m: &str| Error::Corrupt(format!("{name}: {m}"));
    if !bytes.len().is_multiple_of(2) || bytes.len() < 40 {
        return Err(bad("file too short or odd length"));
    }
    let header_len = word_at(bytes, 0).unwrap() as u32;
    let attributes = word_at(bytes, 1).unwrap();
    let status = word_at(bytes, 2).unwrap();
    let num_keys = word_at(bytes, 3).unwrap();
    let num_fields = word_at(bytes, 4).unwrap();
    let record_start = u32_at(bytes, 5).unwrap();
    let file_words = (bytes.len() / 2) as u32;
    if header_len < 20 || header_len > file_words {
        return Err(bad("bad header length"));
    }
    let sig_ok = word_at(bytes, header_len - 2) == Some(0x574f) // "WO"
        && word_at(bytes, header_len - 1) == Some(0x4944); // "ID"
    if !sig_ok {
        return Err(bad("missing WOID signature at end of header"));
    }
    let db_name = u32_at(bytes, 16).filter(|&p| p != 0).and_then(|p| dd_at(bytes, p)).unwrap_or_default();
    let mut rules = Vec::new();
    for i in 0..num_keys as u32 {
        let pn = u32_at(bytes, 20 + 4 * i).ok_or_else(|| bad("truncated rules"))?;
        let pf = u32_at(bytes, 22 + 4 * i).ok_or_else(|| bad("truncated rules"))?;
        rules.push(Rule {
            name: if pn != 0 { dd_at(bytes, pn).unwrap_or_default() } else { String::new() },
            filename: if pf != 0 { sz_at(bytes, pf) } else { None },
        });
    }

    let mut records = BTreeMap::new();
    let mut p = record_start;
    while p < file_words {
        let start = p;
        let flags = word_at(bytes, p).unwrap();
        if flags & 0x8000 == 0 {
            return Err(bad(&format!("record at word {start:#x} lacks ID bit")));
        }
        p += 1;
        let mut fields: Vec<Vec<Vec<u16>>> = Vec::new();
        let mut field: Vec<Vec<u16>> = Vec::new();
        let mut value: Vec<u16> = Vec::new();
        let mut any = false;
        loop {
            let w = word_at(bytes, p).ok_or_else(|| bad("unterminated record"))?;
            p += 1;
            match w {
                ESCAPER => {
                    let nxt = word_at(bytes, p).ok_or_else(|| bad("dangling escape"))?;
                    p += 1;
                    value.push(nxt);
                    any = true;
                }
                BAG_DELIM => {
                    field.push(std::mem::take(&mut value));
                    any = true;
                }
                FIELD_DELIM => {
                    field.push(std::mem::take(&mut value));
                    fields.push(std::mem::take(&mut field));
                    any = true;
                }
                END_OF_RECORD => {
                    if any || !value.is_empty() {
                        field.push(std::mem::take(&mut value));
                        fields.push(std::mem::take(&mut field));
                    }
                    break;
                }
                _ => {
                    value.push(w);
                    any = true;
                }
            }
        }
        records.insert(start, MdbRecord { ptr: start, flags, len: p - start, fields });
    }
    Ok(Mdb { header_len, attributes, status, num_keys, num_fields, record_start, db_name, rules, records, file_words })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaiEntry {
    pub mdb: u32,
    pub pai: u32,
}

pub fn parse_sai(bytes: &[u8], name: &str) -> Result<Vec<SaiEntry>> {
    let bad = |m: &str| Error::Corrupt(format!("{name}: {m}"));
    if u32_at(bytes, 0) != Some(SAI_SIGNATURE) {
        return Err(bad("bad SAI signature"));
    }
    let n = word_at(bytes, 4).ok_or_else(|| bad("truncated"))? as u32;
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let base = 12 + 4 * i;
        let mdb = u32_at(bytes, base).ok_or_else(|| bad("truncated entries"))?;
        let pai = u32_at(bytes, base + 2).ok_or_else(|| bad("truncated entries"))?;
        out.push(SaiEntry { mdb, pai });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct PaiModule {
    /// Word pointer to the module start.
    pub start: u32,
    pub len: u32,
    pub flags: u16,
    pub entries: Vec<u32>,
}

impl PaiModule {
    /// The pointer a SAI record uses for this module (first entry).
    pub fn sai_ptr(&self) -> u32 {
        self.start + 6
    }
}

pub fn parse_pai(bytes: &[u8], name: &str) -> Result<BTreeMap<u32, PaiModule>> {
    let bad = |m: &str| Error::Corrupt(format!("{name}: {m}"));
    if u32_at(bytes, 0) != Some(PAI_SIGNATURE) {
        return Err(bad("bad PAI signature"));
    }
    let file_words = (bytes.len() / 2) as u32;
    let mut out = BTreeMap::new();
    let mut p = 8;
    while p < file_words {
        let len = word_at(bytes, p).unwrap() as u32;
        if len == 0 || !len.is_multiple_of(32) || p + len > file_words {
            return Err(bad(&format!("bad module length {len} at word {p:#x}")));
        }
        let flags = word_at(bytes, p + 1).unwrap();
        let n = word_at(bytes, p + 2).unwrap() as u32;
        if 6 + 2 * n > len {
            return Err(bad(&format!("module at {p:#x} overflows")));
        }
        let entries = (0..n).map(|i| u32_at(bytes, p + 6 + 2 * i).unwrap()).collect();
        let m = PaiModule { start: p, len, flags, entries };
        out.insert(m.sai_ptr(), m);
        p += len;
    }
    Ok(out)
}

/// A child database (artist, albums, ...).
#[derive(Debug, Clone)]
pub struct ChildDb {
    pub rule_name: String,
    pub file_stem: String,
    pub mdb: Mdb,
    pub sai: Vec<SaiEntry>,
    pub pai: BTreeMap<u32, PaiModule>,
    /// Sub-databases of this child (albums → artistalbum on firmware 2.x).
    /// Their PAI modules point at records of this child.
    pub subs: Vec<ChildDb>,
}

impl ChildDb {
    /// Primary values in SAI (menu) order, without the null record.
    pub fn names(&self) -> Vec<String> {
        self.sai.iter().filter_map(|e| self.mdb.records.get(&e.mdb)?.primary()).collect()
    }
}

/// A root database with its children, as stored in one directory.
#[derive(Debug, Clone)]
pub struct Database {
    pub name: String,
    pub mdb: Mdb,
    pub sai: Vec<SaiEntry>,
    pub children: Vec<ChildDb>,
}

/// Load a root database from directory `dir` (device-relative, e.g.
/// `WOID_DB/audio`) on `vfs`; `name` = "audio".  Names match
/// case-insensitively, as FAT may report any case.
pub fn load_database(vfs: &dyn Vfs, dir: &str, name: &str) -> Result<Database> {
    load_database_with(name, |f| {
        let p = crate::vfs::find_file_ci(vfs, dir, f).ok_or_else(|| Error::Missing(crate::vfs::join(dir, f).into()))?;
        vfs.read_file(&p)
    })
}

/// Load a root database from in-memory files (as produced by the builder).
pub fn load_database_from_files(files: &[crate::DbFile], name: &str) -> Result<Database> {
    load_database_with(name, |f| {
        files
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(f))
            .map(|d| d.bytes.clone())
            .ok_or_else(|| Error::Missing(f.into()))
    })
}

fn load_child(fetch: &impl Fn(&str) -> Result<Vec<u8>>, rule: &str, fname: &str, depth: u32) -> Result<ChildDb> {
    let stem = fname.trim_end_matches(".mdb").to_string();
    let mdb = parse_mdb(&fetch(fname)?, fname)?;
    let sai = parse_sai(&fetch(&format!("{stem}.sai"))?, &format!("{stem}.sai"))?;
    let pai = parse_pai(&fetch(&format!("{stem}.pai"))?, &format!("{stem}.pai"))?;
    let mut subs = Vec::new();
    if depth < 2 {
        for r in mdb.rules.iter().skip(1) {
            if let Some(f) = &r.filename {
                subs.push(load_child(fetch, &r.name, f, depth + 1)?);
            }
        }
    }
    Ok(ChildDb { rule_name: rule.to_string(), file_stem: stem, mdb, sai, pai, subs })
}

fn load_database_with(name: &str, fetch: impl Fn(&str) -> Result<Vec<u8>>) -> Result<Database> {
    let mdb = parse_mdb(&fetch(&format!("{name}.mdb"))?, &format!("{name}.mdb"))?;
    let sai = parse_sai(&fetch(&format!("{name}.sai"))?, &format!("{name}.sai"))?;
    let mut children = Vec::new();
    for rule in mdb.rules.iter().skip(1) {
        let Some(fname) = &rule.filename else { continue };
        children.push(load_child(&fetch, &rule.name, fname, 0)?);
    }
    Ok(Database { name: name.to_string(), mdb, sai, children })
}

/// A decoded audio track from an existing database.
#[derive(Debug, Clone)]
pub struct ExistingTrack {
    pub title: String,
    /// Access key values in header order (Playlist, Artist, Album, Genre, Recordings).
    pub keys: Vec<Vec<String>>,
    pub time: u32,
    pub size_kb: u32,
    pub path: String,
    /// 1-based position in its album's track list on the device (keeps the
    /// album order when the database is rebuilt from these records).
    pub album_pos: Option<u32>,
}

impl ExistingTrack {
    /// Convert back into a track for rebuilding (playlists are dropped).
    pub fn to_track(&self) -> crate::Track {
        let key = |k: usize| self.keys.get(k).and_then(|v| v.first()).cloned();
        crate::Track {
            title: self.title.clone(),
            artist: key(1),
            album: key(2),
            genre: key(3),
            recording: key(4),
            disc: None,
            track_no: self.album_pos,
            duration_secs: self.time,
            size_kb: self.size_kb,
            device_path: self.path.clone(),
        }
    }
}

impl Database {
    /// Resolve the audio records (non-null, non-deleted, in SAI order).
    pub fn tracks(&self) -> Vec<ExistingTrack> {
        let nkeys = self.mdb.num_keys as usize;
        let mut album_pos: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
        if let Some(albums) = self.children.iter().find(|c| c.file_stem.eq_ignore_ascii_case("albums")) {
            for e in albums.sai.iter().skip(1) {
                if let Some(m) = albums.pai.get(&e.pai) {
                    for (i, &t) in m.entries.iter().enumerate() {
                        album_pos.entry(t).or_insert(i as u32 + 1);
                    }
                }
            }
        }
        self.sai
            .iter()
            .filter_map(|e| self.mdb.records.get(&e.mdb))
            .filter(|r| !r.is_null() && !r.is_deleted())
            .map(|r| {
                let keys = (1..nkeys)
                    .map(|k| {
                        let child = self.children.get(k - 1);
                        r.pointers(k).into_iter().filter_map(|p| child?.mdb.records.get(&p)?.primary()).collect()
                    })
                    .collect();
                ExistingTrack {
                    title: r.primary().unwrap_or_default(),
                    keys,
                    time: r.u32_field(nkeys).unwrap_or(0),
                    size_kb: r.u32_field(nkeys + 1).unwrap_or(0),
                    path: r.sz_field(nkeys + 2).unwrap_or_default(),
                    album_pos: album_pos.get(&r.ptr).copied(),
                }
            })
            .collect()
    }
}
