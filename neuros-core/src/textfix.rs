//! Repair "?" left in tags by taggers that saved text as Latin-1, which has
//! no curly quotes or dashes: "Don’t" → "Don?t", "80–85" → "80?85".
//!
//! Only patterns that are clear from context are fixed; a "?" that may be
//! a real question mark, or stands for text that can't be guessed (an
//! ellipsis, non-Latin letters), is left alone.

/// Contraction endings after an apostrophe: "Don?t", "It?s", "We?re".
const CONTRACTIONS: &[&str] = &["s", "t", "re", "ve", "ll", "d", "m"];
/// Words that start with an elided apostrophe: "?em", "?Cept", "?Til".
const ELIDED_START: &[&str] = &["em", "cept", "pon", "n", "til", "cause", "round", "tis", "twas", "bout", "neath"];

fn is_word_char(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_ascii_alphanumeric())
}

/// Letters of the word starting at `i` (stops at a non-letter).
fn word_at(chars: &[char], i: usize) -> String {
    chars[i.min(chars.len())..].iter().take_while(|c| c.is_ascii_alphabetic()).collect()
}

/// Letters of the word ending just before `i`.
fn word_before(chars: &[char], i: usize) -> String {
    let mut w: Vec<char> = chars[..i].iter().rev().take_while(|c| c.is_ascii_alphabetic()).copied().collect();
    w.reverse();
    w.into_iter().collect()
}

pub fn repair(s: &str) -> String {
    if !s.contains('?') {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let at = |i: isize| -> Option<char> { (i >= 0 && (i as usize) < n).then(|| chars[i as usize]) };
    // Replacement text for a '?' (by index); default: keep it.
    let mut rep: Vec<Option<&'static str>> = vec![None; n];

    // Runs of 3+ "?" (or a "?" next to another "?") stand for text in another
    // script or real "??" punctuation: leave them.
    let in_run = |i: usize| at(i as isize - 1) == Some('?') || at(i as isize + 1) == Some('?');

    // Quote pairs: "?Hurricane?", "Complete ?B? Sides", "?God Deciding?".
    let opens = |i: usize| {
        let p = at(i as isize - 1);
        (p.is_none() || p == Some(' ') || p == Some('(')) && is_word_char(at(i as isize + 1))
    };
    let closes = |i: usize| {
        let p = at(i as isize - 1);
        let nx = at(i as isize + 1);
        (is_word_char(p) || p == Some('.')) && !is_word_char(nx) && nx != Some('?')
    };
    let mut i = 0;
    while i < n {
        if chars[i] == '?' && !in_run(i) && opens(i) {
            // An elided word ("?em", "?n?", "?78") is not a quote.
            let w = word_at(&chars, i + 1).to_ascii_lowercase();
            let elided = ELIDED_START.contains(&w.as_str()) || at(i as isize + 1).is_some_and(|c| c.is_ascii_digit());
            if !elided && let Some(j) = (i + 1..n).find(|&j| chars[j] == '?' && !in_run(j) && closes(j)) {
                // No other opening quote in between.
                if !(i + 1..j).any(|k| chars[k] == '?' && opens(k)) {
                    rep[i] = Some("'");
                    rep[j] = Some("'");
                    i = j + 1;
                    continue;
                }
            }
        }
        i += 1;
    }

    for i in 0..n {
        if chars[i] != '?' || rep[i].is_some() || in_run(i) {
            continue;
        }
        let p = at(i as isize - 1);
        let nx = at(i as isize + 1);
        let next_word = word_at(&chars, i + 1);
        let prev_word = word_before(&chars, i);
        let lower_next = next_word.to_ascii_lowercase();
        let r: Option<&'static str> = match (p, nx) {
            // 80?85, 1974?1978
            (Some(a), Some(b)) if a.is_ascii_digit() && b.is_ascii_digit() => Some("-"),
            // I?D?I?O?T → I-D-I-O-T (spelled out)
            (Some(a), Some(b))
                if prev_word.len() == 1 && next_word.len() == 1 && a.is_ascii_uppercase() && b.is_ascii_uppercase() =>
            {
                Some("-")
            }
            // C?ur → Coeur (œ)
            (Some('c' | 'C'), Some('u')) if next_word == "ur" && prev_word.len() == 1 => Some("oe"),
            // Don?t, It?s, Shouldn?t?ve, 3?s
            (Some(a), Some(_)) if a.is_ascii_alphanumeric() && CONTRACTIONS.contains(&lower_next.as_str()) => Some("'"),
            // Rank?n?File, Rock ?n? Roll (the opening one)
            (_, Some('n' | 'N')) if next_word.len() == 1 && at(i as isize + 2) == Some('?') => Some("'"),
            // Rank?n?File, Spit n? Drool (the closing one)
            (Some('n' | 'N'), _) if prev_word.len() == 1 => Some("'"),
            // Bag o? Miracles, Ol? Dirty
            (Some(_), Some(' ')) if matches!(prev_word.to_ascii_lowercase().as_str(), "o" | "ol") => Some("'"),
            // d?ici, L?Via, O?Brother, s?ouvre
            (Some(a), Some(b))
                if prev_word.len() == 1
                    && b.is_ascii_alphabetic()
                    && matches!(a.to_ascii_lowercase(), 'd' | 'l' | 's' | 'j' | 'n' | 'm' | 'c' | 't' | 'o')
                    && !(a.is_ascii_uppercase() && a != 'L' && a != 'O' && a != 'D') =>
            {
                Some("'")
            }
            // Heart?Shaped, Astro?Man, La?Z?Boy, U?Mass, Five?O
            (Some(a), Some(b)) if a.is_ascii_alphanumeric() && b.is_ascii_uppercase() => Some("-"),
            // Bringin? It, Comin? Down, Fuckin? (at the end too)
            (Some('n'), None | Some(' ' | ')' | ',')) if prev_word.len() >= 4 && prev_word.ends_with("in") => Some("'"),
            // ?em, ?Cept, ?Pon, ?Til, ?78
            (None | Some(' ' | '('), Some(b))
                if ELIDED_START.contains(&lower_next.as_str())
                    || (b.is_ascii_digit() && word_digits(&chars, i + 1) == 2) =>
            {
                Some("'")
            }
            // The Hives ? Declare Guerre, Singles ? Going Steady
            (Some(' '), Some(' ')) => Some("-"),
            _ => None,
        };
        rep[i] = r;
    }

    let mut s = String::with_capacity(n + 4);
    for (i, &c) in chars.iter().enumerate() {
        match rep[i] {
            Some(r) => s.push_str(r),
            None => s.push(c),
        }
    }
    s
}

fn word_digits(chars: &[char], i: usize) -> usize {
    chars[i..].iter().take_while(|c| c.is_ascii_digit()).count()
}

#[cfg(test)]
mod tests {
    use super::repair;

    #[test]
    fn fixes() {
        for (a, b) in [
            ("Don?t Cry", "Don't Cry"),
            ("I Can?t Give You What I Haven?t Got", "I Can't Give You What I Haven't Got"),
            (
                "Ever Fallen in Love (With Someone You Shouldn?t?ve)",
                "Ever Fallen in Love (With Someone You Shouldn't've)",
            ),
            ("3?s & 7?s", "3's & 7's"),
            ("We?re Only Gonna Die", "We're Only Gonna Die"),
            ("Who?ll Be the Next in Line", "Who'll Be the Next in Line"),
            ("80?85", "80-85"),
            ("The Essential Radio Birdman (1974?1978)", "The Essential Radio Birdman (1974-1978)"),
            ("Heart?Shaped Box", "Heart-Shaped Box"),
            ("Man or Astro?Man?", "Man or Astro-Man?"),
            ("The La?Z?Boy 500", "The La-Z-Boy 500"),
            ("U?Mass", "U-Mass"),
            ("Five?O Ford", "Five-O Ford"),
            ("Bringin? It All Back Home", "Bringin' It All Back Home"),
            ("Cruisin? for a Bruisin?", "Cruisin' for a Bruisin'"),
            ("Stay Off the Fuckin? Flowers", "Stay Off the Fuckin' Flowers"),
            ("Long Before Rock ?n? Roll", "Long Before Rock 'n' Roll"),
            ("Rank?n?File", "Rank'n'File"),
            ("Hail Hail Spit n? Drool", "Hail Hail Spit n' Drool"),
            ("Kill Rock ?n Roll", "Kill Rock 'n Roll"),
            ("Smoke ?em if You Got ?em", "Smoke 'em if You Got 'em"),
            ("Anything ?Cept the Truth", "Anything 'Cept the Truth"),
            ("Natty Dreadlocks ?Pon the Mountain Top", "Natty Dreadlocks 'Pon the Mountain Top"),
            ("Burn My Eye ?78", "Burn My Eye '78"),
            ("Bag o? Miracles", "Bag o' Miracles"),
            ("Blakroc feat. Ludacris & Ol? Dirty Bastard", "Blakroc feat. Ludacris & Ol' Dirty Bastard"),
            ("O?Brother", "O'Brother"),
            ("Duel at the Two O?Clock Bell", "Duel at the Two O'Clock Bell"),
            ("La Femme d?argent", "La Femme d'argent"),
            ("L?Via L?Viaquez", "L'Via L'Viaquez"),
            ("I Belong to You / Mon c?ur s?ouvre a ta voix", "I Belong to You / Mon coeur s'ouvre a ta voix"),
            ("C?ur de pirate", "Coeur de pirate"),
            ("Complete ?B? Sides", "Complete 'B' Sides"),
            ("The Word ?Hurricane?", "The Word 'Hurricane'"),
            ("?God Deciding?", "'God Deciding'"),
            ("There?s No ?i? In Team", "There's No 'i' In Team"),
            ("Echoes on ?A?. Street", "Echoes on 'A'. Street"),
            ("?D? for Dangerous", "'D' for Dangerous"),
            ("The Hives ? Declare Guerre Nucleaire", "The Hives - Declare Guerre Nucleaire"),
            ("Singles ? Going Steady", "Singles - Going Steady"),
            ("Moses? I Amn?t", "Moses? I Amn't"),
            ("a.k.a. I?D?I?O?T", "a.k.a. I-D-I-O-T"),
            ("I?m Not You", "I'm Not You"),
        ] {
            assert_eq!(repair(a), b, "{a}");
        }
    }

    #[test]
    fn leaves_real_question_marks_and_unknowns() {
        for s in [
            "Where Is My Mind?",
            "WTF?",
            "Are You a Hypnotist??",
            "Do You Realize??",
            "What Is Rock!?",
            "Man? or Astro-man?",
            "What?s That Spell?? Go to Hell!",
            "1996 ???????????",
            "???",
            "Dr?im ti miesto",
            "Fuck Armageddon? This is Hell",
            "Suddenly? (I Miss Carpaty)",
            "Borders Are?",
            "Theme From?",
            "Den som spar ?",
            "Sometimes It Feels Like...!?%?*",
            "Should I Stay or Should I Go?",
            "Do They Know it's Christmas? (live 1993)",
        ] {
            let want = s.replace("What?s", "What's");
            assert_eq!(repair(s), want, "{s}");
        }
    }
}

/// One distinct value that `repair` changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChange {
    pub field: crate::overrides::Field,
    pub old: String,
    pub new: String,
    /// Tracks with this value.
    pub tracks: usize,
}

/// Repair title/artist/album/genre of a track; true if anything changed.
pub fn repair_track(t: &mut crate::Track) -> bool {
    let mut changed = false;
    let mut fix = |v: &mut String| {
        let r = repair(v);
        if r != *v {
            *v = r;
            changed = true;
        }
    };
    fix(&mut t.title);
    for v in [&mut t.artist, &mut t.album, &mut t.genre].into_iter().flatten() {
        fix(v);
    }
    changed
}

/// What `repair` would change in these tracks, by distinct value.
pub fn plan(tracks: &[crate::Track]) -> Vec<TextChange> {
    use crate::overrides::Field;
    let mut m: std::collections::BTreeMap<(Field, String), (String, usize)> = Default::default();
    for t in tracks {
        for f in Field::ALL {
            if let Some(v) = f.get(t) {
                let r = repair(v);
                if r != v {
                    m.entry((f, v.to_string())).or_insert((r, 0)).1 += 1;
                }
            }
        }
    }
    m.into_iter().map(|((field, old), (new, tracks))| TextChange { field, old, new, tracks }).collect()
}

/// Repair the text of every track in the device's database and rebuild it.
/// With `opts.dry_run`, only reports.  Your own edits stay on top.
pub fn fix_database(
    vfs: &dyn crate::vfs::Vfs,
    info: &crate::DeviceInfo,
    opts: &crate::WriteOptions,
) -> crate::Result<(Vec<TextChange>, crate::WriteReport)> {
    let db = info.existing.as_ref().ok_or_else(|| crate::Error::Other("no database on the device".into()))?;
    let mut tracks: Vec<crate::Track> = db.tracks().iter().map(|t| t.to_track()).collect();
    let changes = plan(&tracks);
    if changes.is_empty() {
        return Ok((changes, crate::WriteReport::default()));
    }
    for t in &mut tracks {
        repair_track(t);
    }
    let report =
        crate::device::commit(vfs, info, tracks, opts, &info.overrides, crate::WriteReport::default(), |_, _| Ok(()))?;
    Ok((changes, report))
}
