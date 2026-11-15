//! The search language (PLAN.md §6): parsing and compilation to SQL.
//!
//! ```text
//! query := term (' ' term)*          terms are AND-ed
//! term  := ['-'] (key ':' value | text)
//! value := word | "quoted"           commas separate alternatives (OR) where it makes sense
//! ```
//!
//! The compiled condition refers to the aliases `f` (files), `di` (dirs),
//! `fs` (filesystems) and `d` (drives).

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{ToSpan, Zoned};
use rusqlite::Connection;
use rusqlite::types::Value;

use di_model::{FileKind, flags, units};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub negated: bool,
    /// Lowercased filter key, `None` for bare text.
    pub key: Option<String>,
    pub value: String,
    /// Byte span of the whole term in the input, for error highlighting.
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct QueryError {
    pub message: String,
    pub start: usize,
    pub end: usize,
}

impl QueryError {
    fn at(term: &Term, message: impl Into<String>) -> Self {
        QueryError { message: message.into(), start: term.start, end: term.end }
    }
}

/// Filters implemented now.
const KEYS: &[&str] = &[
    "name", "path", "ext", "type", "size", "date", "mtime", "btime", "ctime", "drive", "fs", "fstype",
    "kind", "in",
];

/// Filters that exist in the language but need data from later milestones.
const FUTURE_KEYS: &[(&str, &str)] = &[
    ("online", "device detection (M2)"),
    ("dup", "content hashes (M4)"),
    ("copies", "content hashes (M4)"),
    ("taken", "media metadata (M4)"),
    ("when", "media metadata (M4)"),
    ("camera", "media metadata (M4)"),
    ("lens", "media metadata (M4)"),
    ("iso", "media metadata (M4)"),
    ("focal", "media metadata (M4)"),
    ("aperture", "media metadata (M4)"),
    ("gps", "media metadata (M4)"),
    ("res", "media metadata (M4)"),
    ("width", "media metadata (M4)"),
    ("height", "media metadata (M4)"),
    ("duration", "media metadata (M4)"),
    ("codec", "media metadata (M4)"),
    ("artist", "media metadata (M4)"),
    ("album", "media metadata (M4)"),
    ("title", "media metadata (M4)"),
    ("genre", "media metadata (M4)"),
    ("year", "media metadata (M4)"),
    ("media", "media metadata (M4)"),
];

fn is_flag_key(key: &str) -> bool {
    flags::NAMED.iter().any(|(n, _)| *n == key)
}

fn is_known_key(key: &str) -> bool {
    KEYS.contains(&key) || is_flag_key(key) || FUTURE_KEYS.iter().any(|(k, _)| *k == key)
}

/// Split the input into terms.
pub fn parse(input: &str) -> Result<Vec<Term>, QueryError> {
    let bytes = input.as_bytes();
    let mut terms = Vec::new();
    let mut i = 0;
    let n = bytes.len();

    while i < n {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let mut negated = false;
        if bytes[i] == b'-' && i + 1 < n && !bytes[i + 1].is_ascii_whitespace() {
            negated = true;
            i += 1;
        }

        // Candidate key: [A-Za-z_]+ followed by ':'
        let key_start = i;
        let mut j = i;
        while j < n && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'_') {
            j += 1;
        }
        let mut key = None;
        if j > key_start && j < n && bytes[j] == b':' {
            let k = input[key_start..j].to_ascii_lowercase();
            if !is_known_key(&k) {
                return Err(QueryError {
                    message: format!("unknown filter '{k}:' (known: {})", known_keys_hint()),
                    start,
                    end: j + 1,
                });
            }
            key = Some(k);
            i = j + 1;
        }

        let (value, end) = read_value(input, i, start)?;
        if value.is_empty() {
            return Err(QueryError {
                message: match &key {
                    Some(k) => format!("'{k}:' needs a value"),
                    None => "empty search term".into(),
                },
                start,
                end,
            });
        }
        terms.push(Term { negated, key, value, start, end });
        i = end;
    }
    Ok(terms)
}

fn read_value(input: &str, mut i: usize, term_start: usize) -> Result<(String, usize), QueryError> {
    let bytes = input.as_bytes();
    if i < bytes.len() && bytes[i] == b'"' {
        let close = input[i + 1..].find('"').ok_or(QueryError {
            message: "unterminated quote".into(),
            start: term_start,
            end: input.len(),
        })?;
        let value = input[i + 1..i + 1 + close].to_string();
        let end = i + 1 + close + 1;
        // A quoted value must be followed by whitespace or end of input.
        if end < bytes.len() && !bytes[end].is_ascii_whitespace() {
            return Err(QueryError {
                message: "expected a space after the closing quote".into(),
                start: term_start,
                end,
            });
        }
        return Ok((value, end));
    }
    let start = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    Ok((input[start..i].to_string(), i))
}

fn known_keys_hint() -> String {
    let mut keys: Vec<&str> = KEYS.to_vec();
    keys.extend(["hidden", "trash", "compressed", "enc"]);
    keys.join(", ")
}

/// A compiled WHERE condition with its positional parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    pub sql: String,
    pub params: Vec<Value>,
}

/// Compile a query string to a SQL condition. `conn` is used to expand collections
/// (`in:`) and validate categories.
pub fn compile(conn: &Connection, input: &str) -> Result<Compiled, QueryError> {
    let terms = parse(input)?;
    let mut c = Compiler { conn, params: Vec::new(), depth: 0, tz: TimeZone::system(), now: Zoned::now() };
    let sql = c.terms(&terms)?;
    Ok(Compiled { sql, params: c.params })
}

struct Compiler<'a> {
    conn: &'a Connection,
    params: Vec<Value>,
    depth: usize,
    tz: TimeZone,
    now: Zoned,
}

impl Compiler<'_> {
    fn terms(&mut self, terms: &[Term]) -> Result<String, QueryError> {
        if terms.is_empty() {
            return Ok("1".into());
        }
        let mut parts = Vec::with_capacity(terms.len());
        for t in terms {
            let cond = self.term(t)?;
            parts.push(if t.negated { format!("NOT COALESCE(({cond}), 0)") } else { format!("({cond})") });
        }
        Ok(parts.join(" AND "))
    }

    /// Bind a parameter and return its numbered placeholder (`?N`), so fragments can
    /// be assembled in any order.
    fn p(&mut self, v: impl Into<Value>) -> String {
        self.params.push(v.into());
        format!("?{}", self.params.len())
    }

    fn term(&mut self, t: &Term) -> Result<String, QueryError> {
        let Some(key) = t.key.as_deref() else {
            return Ok(self.name_cond(&t.value));
        };
        if let Some((_, needs)) = FUTURE_KEYS.iter().find(|(k, _)| *k == key) {
            return Err(QueryError::at(t, format!("'{key}:' needs {needs}, which is not built yet")));
        }
        if let Some((_, bit)) = flags::NAMED.iter().find(|(n, _)| *n == key) {
            let on = parse_bool(&t.value).ok_or_else(|| QueryError::at(t, format!("'{key}:' takes yes or no")))?;
            return Ok(format!("(f.flags & {bit}) {} 0", if on { "!=" } else { "=" }));
        }
        match key {
            "name" => Ok(self.name_cond(&t.value)),
            "path" => Ok(self.path_cond(&t.value)),
            "ext" => {
                let exts: Vec<String> = alternatives(&t.value)
                    .map(|e| e.trim_start_matches('.').to_lowercase())
                    .collect();
                Ok(self.in_list("f.ext", exts))
            }
            "type" => {
                let cats: Vec<String> = alternatives(&t.value).map(|c| c.to_lowercase()).collect();
                for cat in &cats {
                    let known: bool = self
                        .conn
                        .query_row("SELECT EXISTS(SELECT 1 FROM ext_categories WHERE category = ?1)", [cat], |r| r.get(0))
                        .unwrap_or(false);
                    if !known {
                        return Err(QueryError::at(t, format!("unknown type '{cat}' (known: {})", self.categories())));
                    }
                }
                let list = self.in_list("category", cats);
                Ok(format!("f.ext IN (SELECT ext FROM ext_categories WHERE {list})"))
            }
            "size" => self.size_cond(t),
            "date" | "mtime" => self.time_cond(t, "f.mtime_ns"),
            "btime" => self.time_cond(t, "f.btime_ns"),
            "ctime" => self.time_cond(t, "f.ctime_ns"),
            "drive" => {
                let mut ors = Vec::new();
                for alt in alternatives(&t.value) {
                    if let Some(id) = parse_short_drive_id(alt) {
                        ors.push(format!("d.id = {}", self.p(id)));
                    } else {
                        let g = self.p(glob_lower(alt));
                        let f1 = self.p(alt.to_string());
                        let f2 = self.p(alt.to_string());
                        let f3 = self.p(alt.to_string());
                        ors.push(format!(
                            "lower(COALESCE(d.label, '')) GLOB {g} OR d.fingerprint = {f1} OR d.serial = {f2} \
                             OR d.id IN (SELECT drive_id FROM drive_identifiers WHERE value = {f3})"
                        ));
                    }
                }
                Ok(ors.join(" OR "))
            }
            "fs" => {
                let mut ors = Vec::new();
                for alt in alternatives(&t.value) {
                    let g = self.p(glob_lower(alt));
                    let u = self.p(alt.to_string());
                    ors.push(format!(
                        "lower(COALESCE(fs.user_label, fs.volume_label, '')) GLOB {g} OR fs.fs_uuid = {u}"
                    ));
                }
                Ok(ors.join(" OR "))
            }
            "fstype" => {
                let types: Vec<String> = alternatives(&t.value).map(|s| s.to_lowercase()).collect();
                Ok(self.in_list("fs.fs_type", types))
            }
            "kind" => {
                let mut codes = Vec::new();
                for alt in alternatives(&t.value) {
                    if alt.eq_ignore_ascii_case("dir") {
                        return Err(QueryError::at(t, "searching directories ('kind:dir') is not built yet"));
                    }
                    let k = FileKind::parse(&alt.to_lowercase()).ok_or_else(|| {
                        QueryError::at(t, format!("unknown kind '{alt}' (file, symlink, fifo, socket, char, block, other)"))
                    })?;
                    codes.push(k.code());
                }
                Ok(self.in_list("f.kind", codes))
            }
            "in" => self.collection(t),
            _ => Err(QueryError::at(t, format!("unknown filter '{key}:'"))),
        }
    }

    fn in_list<T: Into<Value>>(&mut self, column: &str, values: Vec<T>) -> String {
        let marks: Vec<String> = values.into_iter().map(|v| self.p(v)).collect();
        format!("{column} IN ({})", marks.join(", "))
    }

    /// Name substring or glob. Uses the trigram index when the pattern has a run of at
    /// least three literal characters, then checks exactly (the index lookup treats
    /// `%` and `_` in names as wildcards, so it may over-match).
    fn name_cond(&mut self, value: &str) -> String {
        let is_glob = value.contains(['*', '?']);
        let exact = if is_glob {
            format!("lower(f.name) GLOB {}", self.p(glob_lower(value)))
        } else {
            format!("instr(lower(f.name), {}) > 0", self.p(value.to_lowercase()))
        };
        if longest_literal_run(value) >= 3 {
            let like = if is_glob { glob_to_like(value) } else { format!("%{value}%") };
            let fts = format!("f.id IN (SELECT rowid FROM files_fts WHERE files_fts.name LIKE {})", self.p(like));
            format!("{fts} AND {exact}")
        } else {
            exact
        }
    }

    /// Directory path substring or glob, evaluated on the (much smaller) dirs table.
    fn path_cond(&mut self, value: &str) -> String {
        let cond = if value.contains(['*', '?']) {
            format!("lower(path) GLOB {}", self.p(glob_lower(value)))
        } else {
            format!("instr(lower(path), {}) > 0", self.p(value.to_lowercase()))
        };
        format!("f.dir_id IN (SELECT id FROM dirs WHERE {cond})")
    }

    fn size_cond(&mut self, t: &Term) -> Result<String, QueryError> {
        let v = t.value.as_str();
        let size = |s: &str| -> Result<i64, QueryError> {
            units::parse_size(s).map(|b| b as i64).map_err(|e| QueryError::at(t, e))
        };
        if let Some((lo, hi)) = v.split_once("..") {
            let mut parts = Vec::new();
            if !lo.is_empty() {
                parts.push(format!("f.size >= {}", self.p(size(lo)?)));
            }
            if !hi.is_empty() {
                parts.push(format!("f.size <= {}", self.p(size(hi)?)));
            }
            if parts.is_empty() {
                return Err(QueryError::at(t, "empty size range"));
            }
            return Ok(parts.join(" AND "));
        }
        let (op, rest) = split_op(v);
        let n = size(rest)?;
        Ok(format!("f.size {} {}", op.unwrap_or("="), self.p(n)))
    }

    fn time_cond(&mut self, t: &Term, column: &str) -> Result<String, QueryError> {
        let v = t.value.as_str();
        let (lo, hi) = if let Some((a, b)) = v.split_once("..") {
            let lo = if a.is_empty() { None } else { Some(self.period(t, a)?.0) };
            let hi = if b.is_empty() { None } else { Some(self.period(t, b)?.1) };
            if lo.is_none() && hi.is_none() {
                return Err(QueryError::at(t, "empty date range"));
            }
            (lo, hi)
        } else {
            let (op, rest) = split_op(v);
            if let Some(age_point) = self.relative(t, rest)? {
                // Relative values are ages: `<30d` = younger than 30 days.
                match op {
                    None | Some("<") | Some("<=") => (Some(age_point), None),
                    Some(">") | Some(">=") => (None, Some(age_point)),
                    Some(_) => return Err(QueryError::at(t, "use <, <=, > or >= with relative dates")),
                }
            } else {
                let (start, end) = self.period(t, rest)?;
                match op {
                    None | Some("=") => (Some(start), Some(end)),
                    Some(">") => (Some(end), None),
                    Some(">=") => (Some(start), None),
                    Some("<") => (None, Some(start)),
                    Some("<=") => (None, Some(end)),
                    Some(_) => unreachable!(),
                }
            }
        };
        let mut parts = Vec::new();
        if let Some(lo) = lo {
            parts.push(format!("{column} >= {}", self.p(lo)));
        }
        if let Some(hi) = hi {
            parts.push(format!("{column} < {}", self.p(hi)));
        }
        Ok(parts.join(" AND "))
    }

    /// `[start, end)` in ns for an absolute date: `2014`, `2014-07`, `2014-07-12`.
    /// A relative value (`30d`) is a single instant: `(t, t)`.
    fn period(&self, t: &Term, s: &str) -> Result<(i64, i64), QueryError> {
        if let Some(point) = self.relative(t, s)? {
            return Ok((point, point));
        }
        let bad = || QueryError::at(t, format!("invalid date '{s}' (use 2014, 2014-07, 2014-07-12 or 30d/6m/1y)"));
        let parts: Vec<&str> = s.split('-').collect();
        let num = |p: &str, len: usize| -> Result<i64, QueryError> {
            if p.len() != len || !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            p.parse().map_err(|_| bad())
        };
        let (start, end) = match parts.as_slice() {
            [y] => {
                let y = num(y, 4)? as i16;
                let d = Date::new(y, 1, 1).map_err(|_| bad())?;
                (d, d.checked_add(1.year()).map_err(|_| bad())?)
            }
            [y, m] => {
                let d = Date::new(num(y, 4)? as i16, num(m, 2)? as i8, 1).map_err(|_| bad())?;
                (d, d.checked_add(1.month()).map_err(|_| bad())?)
            }
            [y, m, dd] => {
                let d = Date::new(num(y, 4)? as i16, num(m, 2)? as i8, num(dd, 2)? as i8).map_err(|_| bad())?;
                (d, d.checked_add(1.day()).map_err(|_| bad())?)
            }
            _ => return Err(bad()),
        };
        let ns = |d: Date| -> Result<i64, QueryError> {
            let z = d.to_zoned(self.tz.clone()).map_err(|_| bad())?;
            i64::try_from(z.timestamp().as_nanosecond()).map_err(|_| bad())
        };
        Ok((ns(start)?, ns(end)?))
    }

    /// `30d`, `12h`, `2w`, `6m`, `1y` → the instant that long ago, in ns.
    fn relative(&self, t: &Term, s: &str) -> Result<Option<i64>, QueryError> {
        let Some(unit) = s.chars().last() else { return Ok(None) };
        let digits = &s[..s.len() - unit.len_utf8()];
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || !"hdwmy".contains(unit) {
            return Ok(None);
        }
        let n: i64 = digits.parse().map_err(|_| QueryError::at(t, format!("invalid age '{s}'")))?;
        let span = match unit {
            'h' => n.hours(),
            'd' => n.days(),
            'w' => n.weeks(),
            'm' => n.months(),
            _ => n.years(),
        };
        let then = self.now.checked_sub(span).map_err(|_| QueryError::at(t, format!("age out of range '{s}'")))?;
        Ok(Some(i64::try_from(then.timestamp().as_nanosecond()).unwrap_or(i64::MIN)))
    }

    fn collection(&mut self, t: &Term) -> Result<String, QueryError> {
        if self.depth >= 4 {
            return Err(QueryError::at(t, "collections nested too deeply"));
        }
        let query: Option<String> = self
            .conn
            .query_row(
                "SELECT query FROM collections WHERE slug = ?1 OR lower(name) = lower(?1)",
                [&t.value],
                |r| r.get(0),
            )
            .ok();
        let Some(query) = query else {
            return Err(QueryError::at(t, format!("unknown collection '{}'", t.value)));
        };
        let inner = parse(&query).map_err(|e| QueryError::at(t, format!("collection '{}': {}", t.value, e.message)))?;
        self.depth += 1;
        let sql = self
            .terms(&inner)
            .map_err(|e| QueryError::at(t, format!("collection '{}': {}", t.value, e.message)));
        self.depth -= 1;
        sql
    }

    fn categories(&self) -> String {
        let mut stmt = match self.conn.prepare("SELECT DISTINCT category FROM ext_categories ORDER BY 1") {
            Ok(s) => s,
            Err(_) => return String::new(),
        };
        let cats: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default();
        cats.join(", ")
    }
}

fn alternatives(v: &str) -> impl Iterator<Item = &str> {
    v.split(',').map(str::trim).filter(|s| !s.is_empty())
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "yes" | "y" | "true" | "1" | "on" => Some(true),
        "no" | "n" | "false" | "0" | "off" => Some(false),
        _ => None,
    }
}

/// `D3` / `d3` → 3
fn parse_short_drive_id(v: &str) -> Option<i64> {
    let rest = v.strip_prefix(['D', 'd'])?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

fn split_op(v: &str) -> (Option<&'static str>, &str) {
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = v.strip_prefix(op) {
            return (Some(op), rest);
        }
    }
    (None, v)
}

/// Lowercase glob for `lower(col) GLOB ?`, with `[` made literal.
fn glob_lower(v: &str) -> String {
    v.to_lowercase().replace('[', "[[]")
}

fn glob_to_like(v: &str) -> String {
    v.chars()
        .map(|c| match c {
            '*' => '%',
            '?' => '_',
            c => c,
        })
        .collect()
}

fn longest_literal_run(v: &str) -> usize {
    let mut best = 0;
    let mut run = 0;
    for c in v.chars() {
        if matches!(c, '*' | '?' | '%' | '_') {
            run = 0;
        } else {
            run += 1;
            best = best.max(run);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_terms() {
        let t = parse(r#"iceland -ext:cr2,crw size:>10M path:"Photo Library" name:"a b""#).unwrap();
        assert_eq!(t.len(), 5);
        assert_eq!(t[0], Term { negated: false, key: None, value: "iceland".into(), start: 0, end: 7 });
        assert!(t[1].negated);
        assert_eq!(t[1].key.as_deref(), Some("ext"));
        assert_eq!(t[1].value, "cr2,crw");
        assert_eq!(t[2].value, ">10M");
        assert_eq!(t[3].value, "Photo Library");
        assert_eq!(t[4].value, "a b");
    }

    #[test]
    fn parse_errors_have_spans() {
        let e = parse("foo bogus:1").unwrap_err();
        assert!(e.message.contains("unknown filter 'bogus:'"));
        assert_eq!((e.start, e.end), (4, 10));
        assert!(parse("ext:").unwrap_err().message.contains("needs a value"));
        assert!(parse(r#"name:"open"#).unwrap_err().message.contains("unterminated"));
        // A colon after a non-key is just text.
        assert_eq!(parse("10:30").unwrap()[0].key, None);
        // A lone dash is text, not negation.
        assert_eq!(parse("a - b").unwrap()[1].value, "-");
    }

    #[test]
    fn literal_runs() {
        assert_eq!(longest_literal_run("IMG_44*.CR2"), 4);
        assert_eq!(longest_literal_run("IMG_4*"), 3);
        assert_eq!(longest_literal_run("a*b"), 1);
        assert_eq!(glob_to_like("IMG_*.CR?"), "IMG_%.CR_");
    }
}
