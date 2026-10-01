//! Words for searching names, and the search query language.
//!
//! Names are indexed with their camelCase and letter/digit pieces added, so
//! `CharityDairy2023` is found by "dairy" or "2023" and `run1_Sample` by "run".
//! A query is words (all must match, each as a prefix) plus optional filters:
//!
//! ```text
//! dairy methylation            words, anywhere in the name or its folders
//! "raw reads"                  words next to each other
//! type:fastq  ext:csv          file extension (also matches .fastq.gz)
//! is:folder   is:file          kind
//! after:2023  before:2024-06   modified date (year, month, or day)
//! >1GB  <500MB  >=10KB         size (folders: their total size)
//! ```

/// Split one name into its searchable text: the name itself, plus its
/// camelCase and letter/digit pieces when there are any.
pub fn expand(name: &str) -> String {
    let mut out = String::from(name);
    for part in name.split(|c: char| !c.is_alphanumeric()) {
        let pieces = pieces(part);
        if pieces.len() > 1 {
            for p in pieces {
                out.push(' ');
                out.push_str(p);
            }
        }
    }
    out
}

/// "CharityDairy2023" → ["Charity", "Dairy", "2023"]; "HTMLParser" → ["HTML", "Parser"].
fn pieces(s: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut out = Vec::new();
    let mut start = 0;
    for k in 1..chars.len() {
        let (i, c) = chars[k];
        let prev = chars[k - 1].1;
        let next = chars.get(k + 1).map(|x| x.1);
        let boundary = (prev.is_lowercase() && c.is_uppercase())
            || (prev.is_alphabetic() && c.is_numeric())
            || (prev.is_numeric() && c.is_alphabetic())
            || (prev.is_uppercase() && c.is_uppercase() && next.is_some_and(|n| n.is_lowercase()));
        if boundary {
            out.push(&s[start..i]);
            start = i;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub enum Filter {
    /// File extension, lowercase, without the dot.
    Ext(String),
    IsDir,
    IsFile,
    /// Modified at or after this time (seconds since the epoch).
    After(i64),
    /// Modified before this time.
    Before(i64),
    /// Size compared with bytes: (">", 1_000_000_000).
    Size(&'static str, u64),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    /// Plain words; each must match (as a prefix of some word).
    pub words: Vec<String>,
    /// Quoted phrases; their words must appear together.
    pub phrases: Vec<String>,
    pub filters: Vec<Filter>,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.words.is_empty() && self.phrases.is_empty() && self.filters.is_empty()
    }

    /// The SQLite FTS5 match expression, or `None` if there are no words.
    pub fn fts_match(&self) -> Option<String> {
        let q = |s: &str| s.replace('"', "\"\"");
        let mut parts: Vec<String> =
            self.words.iter().filter(|w| w.chars().any(char::is_alphanumeric)).map(|w| format!("\"{}\"*", q(w))).collect();
        parts.extend(self.phrases.iter().filter(|p| p.chars().any(char::is_alphanumeric)).map(|p| format!("\"{}\"", q(p))));
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

pub fn parse(input: &str) -> Query {
    let mut q = Query::default();
    let mut rest = input.trim();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('"') {
            let end = after.find('"').unwrap_or(after.len());
            let phrase = after[..end].trim();
            if !phrase.is_empty() {
                q.phrases.push(phrase.to_string());
            }
            rest = after.get(end + 1..).unwrap_or("").trim_start();
            continue;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let tok = &rest[..end];
        rest = rest[end..].trim_start();
        match filter(tok) {
            Some(f) => q.filters.push(f),
            None => q.words.push(tok.to_string()),
        }
    }
    q
}

fn filter(tok: &str) -> Option<Filter> {
    let lower = tok.to_lowercase();
    if let Some(v) = lower.strip_prefix("type:").or_else(|| lower.strip_prefix("ext:")) {
        let v = v.trim_start_matches('.');
        return (!v.is_empty()).then(|| Filter::Ext(v.to_string()));
    }
    if let Some(v) = lower.strip_prefix("is:") {
        return match v {
            "folder" | "folders" | "dir" | "directory" => Some(Filter::IsDir),
            "file" | "files" => Some(Filter::IsFile),
            _ => None,
        };
    }
    if let Some(v) = lower.strip_prefix("after:") {
        return date(v).map(Filter::After);
    }
    if let Some(v) = lower.strip_prefix("before:") {
        return date(v).map(Filter::Before);
    }
    let s = lower.strip_prefix("size:").unwrap_or(&lower);
    let (op, num) = if let Some(n) = s.strip_prefix(">=") {
        (">=", n)
    } else if let Some(n) = s.strip_prefix("<=") {
        ("<=", n)
    } else if let Some(n) = s.strip_prefix('>') {
        (">", n)
    } else if let Some(n) = s.strip_prefix('<') {
        ("<", n)
    } else {
        return None;
    };
    bytes(num).map(|b| Filter::Size(op, b))
}

/// "1.5GB", "500mb", "10k" → bytes (decimal units, as the app displays sizes).
fn bytes(s: &str) -> Option<u64> {
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: f64 = n.parse().ok()?;
    let mult = match unit.trim_end_matches('b').trim_end_matches("i") {
        "" => 1.0,
        "k" => 1e3,
        "m" => 1e6,
        "g" => 1e9,
        "t" => 1e12,
        "p" => 1e15,
        _ => return None,
    };
    Some((n * mult) as u64)
}

/// "2023", "2023-06", or "2023-06-15" → the start of that period (UTC).
fn date(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next().map(|v| v.parse().ok()).unwrap_or(Some(1))?;
    let d: i64 = it.next().map(|v| v.parse().ok()).unwrap_or(Some(1))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1900..=3000).contains(&y) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400)
}

/// Days since 1970-01-01 (Howard Hinnant's days_from_civil).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Plain words of a query for matching names directly (no index): lowercase,
/// with phrases kept whole.
pub fn name_terms(q: &Query) -> Vec<String> {
    q.words.iter().chain(&q.phrases).map(|w| w.to_lowercase()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_names() {
        assert_eq!(expand("CharityDairy2023.tar"), "CharityDairy2023.tar Charity Dairy 2023");
        assert_eq!(expand("run1_Sample.fastq.gz"), "run1_Sample.fastq.gz run 1");
        assert_eq!(expand("HTMLParser"), "HTMLParser HTML Parser");
        assert_eq!(expand("plain name"), "plain name");
        assert_eq!(expand("part0027"), "part0027 part 0027");
    }

    #[test]
    fn parses_queries() {
        let q = parse(r#"dairy "raw reads" type:.FASTQ is:file after:2023-06 before:2024 >1.5GB size:<=10mb"#);
        assert_eq!(q.words, ["dairy"]);
        assert_eq!(q.phrases, ["raw reads"]);
        assert_eq!(
            q.filters,
            [
                Filter::Ext("fastq".into()),
                Filter::IsFile,
                Filter::After(1_685_577_600),
                Filter::Before(1_704_067_200),
                Filter::Size(">", 1_500_000_000),
                Filter::Size("<=", 10_000_000),
            ]
        );
        assert_eq!(q.fts_match().as_deref(), Some(r#""dairy"* "raw reads""#));
        assert_eq!(parse("type:csv").fts_match(), None);
        assert_eq!(parse(r#"say"hi"#).fts_match().as_deref(), Some(r#""say""hi"*"#));
        assert_eq!(parse("is:nonsense").words, ["is:nonsense"]);
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }
}
