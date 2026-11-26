//! Repair "?" left in tags by taggers that saved text as Latin-1, which has
//! no curly quotes or dashes: "Don’t" → "Don?t", "1990–1999" → "1990?1999".
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

    // Quote pairs: "?Banana?", "Rare ?B? Tracks", "?Quiet Please?".
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
            // 70?79, 1981?1989
            (Some(a), Some(b)) if a.is_ascii_digit() && b.is_ascii_digit() => Some("-"),
            // T?E?S?T → T-E-S-T (spelled out)
            (Some(a), Some(b))
                if prev_word.len() == 1 && next_word.len() == 1 && a.is_ascii_uppercase() && b.is_ascii_uppercase() =>
            {
                Some("-")
            }
            // C?ur → Coeur (œ)
            (Some('c' | 'C'), Some('u')) if next_word == "ur" && prev_word.len() == 1 => Some("oe"),
            // Don?t, It?s, Shouldn?t?ve, 3?s
            (Some(a), Some(_)) if a.is_ascii_alphanumeric() && CONTRACTIONS.contains(&lower_next.as_str()) => Some("'"),
            // Pick?n?Mix, Rock ?n? Roll (the opening one)
            (_, Some('n' | 'N')) if next_word.len() == 1 && at(i as isize + 2) == Some('?') => Some("'"),
            // Pick?n?Mix, Fish n? Chips (the closing one)
            (Some('n' | 'N'), _) if prev_word.len() == 1 => Some("'"),
            // Cup o? Tea, Ol? Rusty
            (Some(_), Some(' ')) if matches!(prev_word.to_ascii_lowercase().as_str(), "o" | "ol") => Some("'"),
            // d?ami, L?Etoile, O?Malley, s?endort
            (Some(a), Some(b))
                if prev_word.len() == 1
                    && b.is_ascii_alphabetic()
                    && matches!(a.to_ascii_lowercase(), 'd' | 'l' | 's' | 'j' | 'n' | 'm' | 'c' | 't' | 'o')
                    && !(a.is_ascii_uppercase() && a != 'L' && a != 'O' && a != 'D') =>
            {
                Some("'")
            }
            // Star?Shaped, Robo?Men, Go?K?Art, X?Ray, Nine?O
            (Some(a), Some(b)) if a.is_ascii_alphanumeric() && b.is_ascii_uppercase() => Some("-"),
            // Walkin? Home, Movin? On, Groovin? (at the end too)
            (Some('n'), None | Some(' ' | ')' | ',')) if prev_word.len() >= 4 && prev_word.ends_with("in") => Some("'"),
            // ?em, ?Cept, ?Pon, ?Til, ?78
            (None | Some(' ' | '('), Some(b))
                if ELIDED_START.contains(&lower_next.as_str())
                    || (b.is_ascii_digit() && word_digits(&chars, i + 1) == 2) =>
            {
                Some("'")
            }
            // Live ? at Home, Singles ? Volume Two
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
            ("Don?t Wait for the Bus", "Don't Wait for the Bus"),
            ("I Can?t Find What I Haven?t Lost", "I Can't Find What I Haven't Lost"),
            ("You Shouldn?t?ve Called (Again)", "You Shouldn't've Called (Again)"),
            ("4?s & 9?s", "4's & 9's"),
            ("We?re Late Again", "We're Late Again"),
            ("Who?ll Feed the Cat", "Who'll Feed the Cat"),
            ("70?79", "70-79"),
            ("The Collected Tests (1981?1989)", "The Collected Tests (1981-1989)"),
            ("Star?Shaped Lamp", "Star-Shaped Lamp"),
            ("Band of Robo?Men?", "Band of Robo-Men?"),
            ("The Go?K?Art Race", "The Go-K-Art Race"),
            ("X?Ray Eyes", "X-Ray Eyes"),
            ("Nine?O Diner", "Nine-O Diner"),
            ("Walkin? Home Slow", "Walkin' Home Slow"),
            ("Movin? and Groovin?", "Movin' and Groovin'"),
            ("Stop the Darnin? Noise", "Stop the Darnin' Noise"),
            ("Long Before Rock ?n? Roll Day", "Long Before Rock 'n' Roll Day"),
            ("Pick?n?Mix", "Pick'n'Mix"),
            ("Fish n? Chips Night", "Fish n' Chips Night"),
            ("Test Rock ?n Roll", "Test Rock 'n Roll"),
            ("Wake ?em Up, Shake ?em Down", "Wake 'em Up, Shake 'em Down"),
            ("Anything ?Cept Monday", "Anything 'Cept Monday"),
            ("Stay ?Pon the Hill", "Stay 'Pon the Hill"),
            ("Back in ?82", "Back in '82"),
            ("Cup o? Tea", "Cup o' Tea"),
            ("Ol? Rusty Truck", "Ol' Rusty Truck"),
            ("O?Malley", "O'Malley"),
            ("Five O?Clock Train", "Five O'Clock Train"),
            ("La Maison d?ami", "La Maison d'ami"),
            ("L?Etoile L?Ombre", "L'Etoile L'Ombre"),
            ("Un Jour / Un c?ur s?endort", "Un Jour / Un coeur s'endort"),
            ("C?ur en carton", "Coeur en carton"),
            ("Rare ?B? Tracks", "Rare 'B' Tracks"),
            ("The Word ?Banana?", "The Word 'Banana'"),
            ("?Quiet Please?", "'Quiet Please'"),
            ("There?s No ?x? In Test", "There's No 'x' In Test"),
            ("Meet on ?C?. Street", "Meet on 'C'. Street"),
            ("?K? for Kettle", "'K' for Kettle"),
            ("The Examples ? Live at Home", "The Examples - Live at Home"),
            ("Singles ? Volume Two", "Singles - Volume Two"),
            ("Really? I Amn?t", "Really? I Amn't"),
            ("a.k.a. T?E?S?T", "a.k.a. T-E-S-T"),
            ("I?m Not a Robot", "I'm Not a Robot"),
        ] {
            assert_eq!(repair(a), b, "{a}");
        }
    }

    #[test]
    fn leaves_real_question_marks_and_unknowns() {
        for s in [
            "Where Are My Keys?",
            "WTF?",
            "Are You a Robot??",
            "Do You See It??",
            "What Is Jazz!?",
            "Robo? or Robo-Man?",
            "What?s That Noise?? Go Home!",
            "1996 ???????????",
            "???",
            "Vr?ta mesto",
            "Forget It? This is Fine",
            "Suddenly? (I Miss Lunch)",
            "Lunch Is?",
            "Theme From?",
            "Hej och ?",
            "It Feels Like...!?%?*",
            "Should I Test or Should I Go?",
            "Do They Know it's Tuesday? (live 1993)",
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
