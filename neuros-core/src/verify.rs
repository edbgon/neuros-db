//! Consistency checks for a root database and its children.  Run on every
//! database we build before it is written to the device, and available
//! for checking an existing device database.

use std::collections::{BTreeMap, BTreeSet};

use crate::reader::{Database, Mdb, SaiEntry};

/// Prefix for issues that real, working device databases (made by
/// NeurosDBM) have too; the firmware tolerates them.  Our own builds must
/// not have any.
pub const NOTE: &str = "note: ";

pub fn is_note(msg: &str) -> bool {
    msg.starts_with(NOTE)
}

fn check_sai(name: &str, mdb: &Mdb, sai: &[SaiEntry], problems: &mut Vec<String>) {
    let live: BTreeSet<u32> = mdb.records.values().filter(|r| !r.is_deleted()).map(|r| r.ptr).collect();
    let listed: BTreeSet<u32> = sai.iter().map(|e| e.mdb).collect();
    if listed.len() != sai.len() {
        problems.push(format!("{name}.sai: duplicate entries"));
    }
    for e in sai {
        if !mdb.records.contains_key(&e.mdb) {
            problems.push(format!("{name}.sai: pointer {:#x} is not a record start", e.mdb));
        }
    }
    if live != listed {
        problems.push(format!(
            "{NOTE}{name}.sai: lists {} records but MDB has {} live records",
            listed.len(),
            live.len()
        ));
    }
    match sai.first().and_then(|e| mdb.records.get(&e.mdb)) {
        Some(r) if r.is_null() => {}
        _ => problems.push(format!("{name}.sai: first entry is not the null record")),
    }
    if mdb.records.values().filter(|r| r.is_null()).count() != 1 {
        problems.push(format!("{name}.mdb: expected exactly one null record"));
    }
}

/// Returns a list of problems; empty means the database is consistent.
pub fn verify(db: &Database) -> Vec<String> {
    let mut p = Vec::new();
    let root = &db.mdb;
    if root.status & 1 == 0 {
        p.push(format!("{}.mdb: isModified bit not set (firmware will ignore it)", db.name));
    }
    check_sai(&db.name, root, &db.sai, &mut p);

    let nkeys = root.num_keys as usize;
    let nfields = root.num_fields as usize;
    if db.children.len() != nkeys - 1 {
        p.push(format!("{}: {} children but {} access keys", db.name, db.children.len(), nkeys - 1));
        return p;
    }

    // Forward links: child ptr -> set of root records, per key.
    let mut forward: Vec<BTreeMap<u32, BTreeSet<u32>>> = vec![BTreeMap::new(); nkeys - 1];
    for r in root.records.values().filter(|r| !r.is_null() && !r.is_deleted()) {
        if r.fields.len() != nfields {
            p.push(format!("{}.mdb: record {:#x} has {} fields, expected {nfields}", db.name, r.ptr, r.fields.len()));
            continue;
        }
        for k in 1..nkeys {
            let ptrs = r.pointers(k);
            if ptrs.is_empty() {
                p.push(format!("{}.mdb: record {:#x} key {k} is empty", db.name, r.ptr));
            }
            for c in ptrs {
                if !db.children[k - 1].mdb.records.contains_key(&c) {
                    p.push(format!("{}.mdb: record {:#x} key {k} points at {c:#x}, not a record", db.name, r.ptr));
                }
                forward[k - 1].entry(c).or_default().insert(r.ptr);
            }
        }
    }

    for (k, child) in db.children.iter().enumerate() {
        let cname = &child.file_stem;
        check_sai(cname, &child.mdb, &child.sai, &mut p);
        let used_modules: BTreeSet<u32> = child.sai.iter().map(|e| e.pai).filter(|&x| x != 0).collect();
        for m in child.pai.values() {
            if !used_modules.contains(&m.sai_ptr()) {
                p.push(format!("{cname}.pai: module at {:#x} not referenced by SAI", m.start));
            }
            // Firmware itself leaves the flag clear on empty modules, so
            // only "marked empty but has entries" is an error.
            if m.flags & 1 != 0 && !m.entries.is_empty() {
                p.push(format!("{cname}.pai: module at {:#x} empty flag wrong", m.start));
            }
        }
        for e in &child.sai {
            let back: BTreeSet<u32> = if e.pai == 0 {
                BTreeSet::new()
            } else if let Some(m) = child.pai.get(&e.pai) {
                m.entries.iter().copied().collect()
            } else {
                p.push(format!("{cname}.sai: PAI pointer {:#x} is not a module entry start", e.pai));
                continue;
            };
            let fwd = forward[k].get(&e.mdb).cloned().unwrap_or_default();
            if back != fwd {
                let is_null = child.mdb.records.get(&e.mdb).is_some_and(|r| r.is_null());
                p.push(format!(
                    "{}{cname}: record {:#x} back-links {} tracks, but {} tracks point at it",
                    if is_null { NOTE } else { "" },
                    e.mdb,
                    back.len(),
                    fwd.len()
                ));
            }
        }
    }

    for child in &db.children {
        for sub in &child.subs {
            verify_sub(child, sub, &mut p);
        }
    }
    p
}

/// A sub-database (e.g. artistalbum under albums): its PAI modules point at
/// records of the parent child database.
fn verify_sub(parent: &crate::reader::ChildDb, sub: &crate::reader::ChildDb, p: &mut Vec<String>) {
    let name = &sub.file_stem;
    check_sai(name, &sub.mdb, &sub.sai, p);
    let mut seen: BTreeMap<u32, usize> = BTreeMap::new();
    for e in &sub.sai {
        let Some(rec) = sub.mdb.records.get(&e.mdb) else { continue };
        let entries = match (e.pai, sub.pai.get(&e.pai)) {
            (0, _) => Vec::new(),
            (_, Some(m)) => m.entries.clone(),
            (ptr, None) => {
                p.push(format!("{name}.sai: PAI pointer {ptr:#x} is not a module entry start"));
                continue;
            }
        };
        if !rec.is_null() && entries.is_empty() {
            p.push(format!("{name}: {:?} lists no {}", rec.primary().unwrap_or_default(), parent.file_stem));
        }
        for t in entries {
            match parent.mdb.records.get(&t) {
                Some(r) if !r.is_null() && !r.is_deleted() => *seen.entry(t).or_default() += 1,
                _ => p.push(format!("{name}: entry {t:#x} is not a live {} record", parent.file_stem)),
            }
        }
    }
    let dup = seen.values().filter(|&&n| n > 1).count();
    if dup > 0 {
        p.push(format!("{NOTE}{name}: {dup} {} records are listed under more than one entry", parent.file_stem));
    }
}
