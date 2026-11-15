//! Search-box assistance: what is being typed at the cursor, a hint for it, and
//! Tab-completion candidates (filter keys and their values).

use rusqlite::Connection;

use di_core::filters::{self, FilterDoc};

/// Values from the catalog for completing filter values.
#[derive(Debug, Default, Clone)]
pub struct Vocab {
    pub categories: Vec<String>,
    /// (slug, name)
    pub collections: Vec<(String, String)>,
    /// (id, label)
    pub drives: Vec<(i64, Option<String>)>,
    pub fs_labels: Vec<String>,
    pub fstypes: Vec<String>,
    /// Most common first.
    pub exts: Vec<String>,
    pub cameras: Vec<String>,
    pub lenses: Vec<String>,
    pub codecs: Vec<String>,
    pub artists: Vec<String>,
    pub albums: Vec<String>,
    pub genres: Vec<String>,
}

fn strings(conn: &Connection, sql: &str) -> Vec<String> {
    let Ok(mut stmt) = conn.prepare(sql) else { return Vec::new() };
    stmt.query_map([], |r| r.get::<_, String>(0))
        .map(|rows| rows.filter_map(Result::ok).filter(|s| !s.trim().is_empty()).collect())
        .unwrap_or_default()
}

impl Vocab {
    pub fn load(conn: &Connection) -> Vocab {
        let pairs = |sql: &str| -> Vec<(String, String)> {
            let Ok(mut stmt) = conn.prepare(sql) else { return Vec::new() };
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(|rows| rows.filter_map(Result::ok).collect())
                .unwrap_or_default()
        };
        let drives = conn
            .prepare("SELECT id, label FROM drives ORDER BY id")
            .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>())
            .unwrap_or_default();
        const CAP: &str = "LIMIT 5000";
        Vocab {
            categories: strings(conn, "SELECT DISTINCT category FROM ext_categories ORDER BY 1"),
            collections: pairs("SELECT slug, name FROM collections ORDER BY id"),
            drives,
            fs_labels: strings(conn, "SELECT DISTINCT label FROM filesystems WHERE label IS NOT NULL ORDER BY 1"),
            fstypes: strings(conn, "SELECT DISTINCT fs_type FROM filesystems ORDER BY 1"),
            exts: strings(conn, "SELECT ext FROM files WHERE ext IS NOT NULL AND ext <> '' GROUP BY ext ORDER BY count(*) DESC LIMIT 400"),
            cameras: strings(
                conn,
                &format!(
                    "SELECT v FROM (SELECT camera_model AS v FROM media UNION SELECT camera_make FROM media)
                     WHERE v IS NOT NULL ORDER BY v {CAP}"
                ),
            ),
            lenses: strings(conn, &format!("SELECT DISTINCT lens_model FROM media WHERE lens_model IS NOT NULL ORDER BY 1 {CAP}")),
            codecs: strings(conn, "SELECT DISTINCT codec FROM media_streams WHERE codec IS NOT NULL ORDER BY 1"),
            artists: strings(
                conn,
                &format!(
                    "SELECT v FROM (SELECT artist AS v FROM media UNION SELECT album_artist FROM media)
                     WHERE v IS NOT NULL ORDER BY v {CAP}"
                ),
            ),
            albums: strings(conn, &format!("SELECT DISTINCT album FROM media WHERE album IS NOT NULL ORDER BY 1 {CAP}")),
            genres: strings(conn, &format!("SELECT DISTINCT genre FROM media WHERE genre IS NOT NULL ORDER BY 1 {CAP}")),
        }
    }
}

/// What the cursor is in. Positions are char indices into the query.
#[derive(Debug, Clone, PartialEq)]
pub enum Context {
    /// Between terms (or an empty query).
    Between,
    /// A bare word, possibly the start of a filter key. `start..end` is what a
    /// completed key replaces (the word, including a `:` if already typed).
    Word { text: String, start: usize, end: usize },
    /// The value of a known filter. `start..end` is the part a completion replaces.
    Value { key: String, prefix: String, start: usize, end: usize },
    /// A key that is not a filter.
    UnknownKey { key: String, suggestion: Option<&'static str>, start: usize, end: usize },
}

/// Token spans (char indices), splitting on whitespace outside quotes.
fn tokens(chars: &[char]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let mut quoted = false;
        while i < chars.len() && (quoted || !chars[i].is_whitespace()) {
            if chars[i] == '"' {
                quoted = !quoted;
            }
            i += 1;
        }
        out.push((start, i));
    }
    out
}

fn is_known(key: &str) -> bool {
    filters::doc(key).is_some() || key == "online"
}

pub fn context(text: &str, cursor: usize) -> Context {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let Some(&(start, end)) = tokens(&chars).iter().find(|(s, e)| *s <= cursor && cursor <= *e) else {
        return Context::Between;
    };
    let mut ks = start;
    if chars[ks] == '-' {
        ks += 1;
    }
    let mut j = ks;
    while j < end && (chars[j].is_ascii_alphabetic() || chars[j] == '_') {
        j += 1;
    }
    let has_key = j > ks && j < end && chars[j] == ':';
    if !has_key || cursor <= j {
        if ks >= cursor && !has_key {
            return Context::Between;
        }
        let word: String = chars[ks..cursor.max(ks)].iter().collect();
        let replace_end = if has_key { j + 1 } else { end };
        return Context::Word { text: word, start: ks, end: replace_end };
    }
    let key: String = chars[ks..j].iter().collect::<String>().to_ascii_lowercase();
    if !is_known(&key) {
        return Context::UnknownKey { suggestion: filters::closest_key(&key), key, start: ks, end: j + 1 };
    }
    // The value: after the colon; a leading quote makes it one segment.
    let vstart = j + 1;
    if chars.get(vstart) == Some(&'"') {
        let prefix: String = chars[vstart + 1..cursor.max(vstart + 1)].iter().filter(|c| **c != '"').collect();
        return Context::Value { key, prefix, start: vstart, end };
    }
    // Comma-separated alternatives: complete the one at the cursor.
    let mut seg_start = vstart;
    for (i, c) in chars.iter().enumerate().take(cursor).skip(vstart) {
        if *c == ',' {
            seg_start = i + 1;
        }
    }
    let seg_end = (cursor..end).find(|i| chars[*i] == ',').unwrap_or(end);
    // Comparison operators are kept; only the value after them is completed.
    while seg_start < cursor && matches!(chars[seg_start], '<' | '>' | '=') {
        seg_start += 1;
    }
    let prefix: String = chars[seg_start..cursor].iter().collect();
    Context::Value { key, prefix, start: seg_start, end: seg_end }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Text inserted into the query.
    pub insert: String,
    /// Shown in the hint line.
    pub display: String,
}

fn quote_if_needed(v: &str) -> String {
    if v.chars().any(|c| c.is_whitespace() || c == ',' || c == ':') {
        format!("\"{}\"", v.replace('"', ""))
    } else {
        v.to_string()
    }
}

/// Matches for `prefix`: prefix matches first, then substring matches.
fn matching(items: impl Iterator<Item = (String, String)> + Clone, prefix: &str) -> Vec<Candidate> {
    let p = prefix.to_lowercase();
    let mut out: Vec<Candidate> = Vec::new();
    let mut push = |insert: String, display: String| {
        if !out.iter().any(|c| c.insert.eq_ignore_ascii_case(&insert)) {
            out.push(Candidate { insert, display });
        }
    };
    for (v, d) in items.clone() {
        if v.to_lowercase().starts_with(&p) {
            push(v, d);
        }
    }
    // Substring matches only from two characters on (one letter matches nearly everything).
    if p.chars().count() >= 2 {
        for (v, d) in items {
            if v.to_lowercase().contains(&p) {
                push(v, d);
            }
        }
    }
    out
}

/// Completion candidates for the context.
pub fn candidates(ctx: &Context, vocab: &Vocab) -> Vec<Candidate> {
    match ctx {
        Context::Between => Vec::new(),
        Context::Word { text, .. } if text.is_empty() => Vec::new(),
        Context::Word { text, .. } => filters::complete_key(text)
            .into_iter()
            .map(|k| Candidate { insert: format!("{k}:"), display: format!("{k}:") })
            .collect(),
        Context::UnknownKey { suggestion, .. } => suggestion
            .map(|s| vec![Candidate { insert: format!("{s}:"), display: format!("{s}:") }])
            .unwrap_or_default(),
        Context::Value { key, prefix, .. } => {
            let doc = filters::doc(key);
            let fixed = doc.map(|d| d.values).unwrap_or_default().iter().map(|v| (v.to_string(), v.to_string()));
            let plain = |v: &Vec<String>| v.iter().map(|s| (s.clone(), s.clone())).collect::<Vec<_>>();
            let dynamic: Vec<(String, String)> = match key.as_str() {
                "type" => plain(&vocab.categories),
                "in" => vocab.collections.iter().map(|(s, n)| (s.clone(), format!("{s} ({n})"))).collect(),
                "drive" => vocab
                    .drives
                    .iter()
                    .map(|(id, l)| (format!("D{id}"), match l { Some(l) => format!("D{id} {l}"), None => format!("D{id}") }))
                    .collect(),
                "fs" => plain(&vocab.fs_labels),
                "fstype" => plain(&vocab.fstypes),
                "ext" => plain(&vocab.exts),
                "camera" => plain(&vocab.cameras),
                "lens" => plain(&vocab.lenses),
                "codec" => plain(&vocab.codecs),
                "artist" => plain(&vocab.artists),
                "album" => plain(&vocab.albums),
                "genre" => plain(&vocab.genres),
                _ => Vec::new(),
            };
            let all: Vec<(String, String)> = fixed.chain(dynamic).collect();
            let mut out = matching(all.into_iter(), prefix);
            // Don't offer exactly what is already typed as the only choice.
            if out.len() == 1 && out[0].insert.eq_ignore_ascii_case(prefix) {
                out.clear();
            }
            for c in &mut out {
                c.insert = quote_if_needed(&c.insert);
            }
            out
        }
    }
}

/// The range `apply` replaces for this context.
pub fn replace_range(ctx: &Context) -> Option<(usize, usize)> {
    match ctx {
        Context::Between => None,
        Context::Word { start, end, .. } | Context::Value { start, end, .. } | Context::UnknownKey { start, end, .. } => {
            Some((*start, *end))
        }
    }
}

/// A one- or two-part hint for the context: (emphasized, plain).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub head: String,
    pub text: String,
    /// Shown when there is nothing to complete.
    pub examples: Vec<&'static str>,
    pub warning: bool,
}

pub fn hint(ctx: &Context, query_empty: bool) -> Hint {
    let doc_hint = |d: &FilterDoc| Hint { head: d.syntax.to_string(), text: d.help.to_string(), examples: d.examples.to_vec(), warning: false };
    match ctx {
        Context::Between if query_empty => Hint {
            head: "Search".into(),
            text: "words match file names; key:value filters narrow it down. Tab completes, F1 lists every filter.".into(),
            examples: vec!["ext:cr2,crw date:2014", "in:photos-raw copies:1", "camera:canon"],
            warning: false,
        },
        Context::Between => Hint {
            head: "Add a term".into(),
            text: "terms are AND-ed; -term excludes. ⏎/↓ go to the results, F1 lists every filter.".into(),
            examples: Vec::new(),
            warning: false,
        },
        Context::Word { text, .. } => {
            let keys = filters::complete_key(text);
            match keys.as_slice() {
                [k] => filters::doc(k).map(doc_hint).unwrap_or_else(|| name_hint(text)),
                [] => name_hint(text),
                _ => Hint {
                    head: format!("{text}…"),
                    text: "a name search, or one of these filters (Tab cycles):".into(),
                    examples: Vec::new(),
                    warning: false,
                },
            }
        }
        Context::Value { key, .. } => match filters::doc(key) {
            Some(d) => doc_hint(d),
            None => Hint { head: format!("{key}:"), text: "not available yet".into(), examples: Vec::new(), warning: true },
        },
        Context::UnknownKey { key, suggestion, .. } => Hint {
            head: format!("{key}: is not a filter"),
            text: match suggestion {
                Some(s) => format!("did you mean {s}:? Tab fixes it. F1 lists every filter."),
                None => "F1 lists every filter.".into(),
            },
            examples: Vec::new(),
            warning: true,
        },
    }
}

fn name_hint(text: &str) -> Hint {
    let glob = text.contains('*') || text.contains('?');
    Hint {
        head: if glob { format!("name matches {text}") } else { format!("name contains \"{text}\"") },
        text: "case-insensitive; * and ? are wildcards, quotes keep spaces".into(),
        examples: Vec::new(),
        warning: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(q: &str) -> Context {
        // The cursor is at '|', or at the end.
        match q.find('|') {
            Some(i) => context(&q.replace('|', ""), q[..i].chars().count()),
            None => context(q, q.chars().count()),
        }
    }

    #[test]
    fn contexts() {
        assert_eq!(ctx(""), Context::Between);
        assert_eq!(ctx("ext:cr2 "), Context::Between);
        assert_eq!(ctx("cam"), Context::Word { text: "cam".into(), start: 0, end: 3 });
        assert_eq!(ctx("-cam"), Context::Word { text: "cam".into(), start: 1, end: 4 });
        assert_eq!(ctx("ca|mera:canon"), Context::Word { text: "ca".into(), start: 0, end: 7 });
        assert_eq!(ctx("camera:ca"), Context::Value { key: "camera".into(), prefix: "ca".into(), start: 7, end: 9 });
        assert_eq!(ctx("x ext:cr2,c"), Context::Value { key: "ext".into(), prefix: "c".into(), start: 10, end: 11 });
        assert_eq!(ctx("ext:c|r2,jpg"), Context::Value { key: "ext".into(), prefix: "c".into(), start: 4, end: 7 });
        assert_eq!(ctx("res:>=4"), Context::Value { key: "res".into(), prefix: "4".into(), start: 6, end: 7 });
        assert_eq!(ctx("camera:\"eos 5"), Context::Value { key: "camera".into(), prefix: "eos 5".into(), start: 7, end: 13 });
        assert!(matches!(ctx("camra:x"), Context::UnknownKey { suggestion: Some("camera"), start: 0, end: 6, .. }));
    }

    #[test]
    fn completion() {
        let v = Vocab {
            cameras: vec!["Canon EOS 5D Mark III".into(), "iPhone 12".into()],
            collections: vec![("photos-raw".into(), "RAW photos".into())],
            ..Default::default()
        };
        let ins = |q: &str| candidates(&ctx(q), &v).into_iter().map(|c| c.insert).collect::<Vec<_>>();
        assert_eq!(ins("siz"), vec!["size:"]);
        assert_eq!(ins("camera:5d"), vec!["\"Canon EOS 5D Mark III\""]);
        assert_eq!(ins("in:"), vec!["photos-raw"]);
        assert_eq!(ins("dup:l"), vec!["likely"]);
        assert_eq!(ins("camra:"), vec!["camera:"]);
        assert!(ins("gps:yes").is_empty(), "nothing to add to a complete value");
        assert!(ins("").is_empty());
    }
}
