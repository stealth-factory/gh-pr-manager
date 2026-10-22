//! Universal search: query parsing, local scoring, highlights, snippets and completion.
//! Pure (no `App`), so all of it is unit-tested below. See docs/search.md.

use std::ops::Range;

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32String};

use crate::github::{PrSummary, login_of};

/// Characters of a stripped description kept for matching (≈ 1.9 KB average).
pub const BODY_CAP: usize = 2000;

pub fn matcher() -> Matcher {
    Matcher::new(Config::DEFAULT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Number,
    Title,
    Repo,
    Branch,
    Author,
    Labels,
    Body,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsFlag {
    Draft,
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiFlag {
    Failing,
    Passing,
    Pending,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewFlag {
    Approved,
    Changes,
    Required,
    None,
}

#[derive(Debug, Clone)]
pub enum Token {
    /// A free word: fuzzy over the short fields, substring over the body.
    Term(Atom, String),
    /// A `"quoted phrase"`: substring everywhere.
    Phrase(Atom, String),
    /// `#123` or `123`: exact / prefix on the number (and plain text outside number mode).
    Number(u64, Atom),
    Repo(String, bool),
    Author(String, bool),
    Label(String, bool),
    Head(String),
    Is(IsFlag, bool),
    Ci(CiFlag),
    Review(ReviewFlag),
    /// A known prefix with a value it doesn't take (`is:foo`); ignored, drawn red.
    Invalid,
    /// A known prefix with no value yet (`repo:`); ignored.
    Pending,
}

impl Token {
    pub fn is_free(&self) -> bool {
        matches!(self, Token::Term(..) | Token::Phrase(..) | Token::Number(..))
    }

    pub fn is_filter(&self) -> bool {
        matches!(self, Token::Repo(..) | Token::Author(..) | Token::Label(..) | Token::Head(_) | Token::Is(..) | Token::Ci(_) | Token::Review(_))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Query {
    pub tokens: Vec<Token>,
    /// Byte range of each token in `raw`.
    pub spans: Vec<Range<usize>>,
    pub raw: String,
    /// Starts with a digit or `#`: only PR numbers are matched and other sections hide.
    pub number_mode: bool,
    /// Tokens followed by whitespace (or closed phrases): words GitHub can search for.
    pub complete: usize,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// The number typed in number mode.
    pub fn number(&self) -> Option<u64> {
        match self.tokens.first() {
            Some(Token::Number(n, _)) if self.number_mode => Some(*n),
            _ => None,
        }
    }

    pub fn has_free(&self) -> bool {
        self.tokens.iter().any(Token::is_free)
    }
}

fn atom(s: &str, kind: AtomKind) -> Atom {
    Atom::new(s, CaseMatching::Ignore, Normalization::Smart, kind, false)
}

/// Lowercase one char for one char, so char indices line up with the original.
pub fn lower(s: &str) -> String {
    s.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect()
}

const PREFIXES: [&str; 8] = ["repo:", "author:", "label:", "head:", "branch:", "is:", "ci:", "review:"];

/// Prefixes that accept a leading `-`. `head`/`ci`/`review` have no negated form.
fn negatable(prefix: &str) -> bool {
    matches!(prefix, "repo:" | "author:" | "label:" | "is:")
}

fn classify(word: &str, me: Option<&str>) -> Token {
    let (neg, body) = match word.strip_prefix('-') {
        Some(rest) if PREFIXES.iter().any(|p| rest.to_ascii_lowercase().starts_with(p)) => (true, rest),
        _ => (false, word),
    };
    if let Some(digits) = body.strip_prefix('#').filter(|d| !neg && !d.is_empty() && d.len() <= 9 && d.bytes().all(|b| b.is_ascii_digit())) {
        return Token::Number(digits.parse().unwrap_or(0), atom(digits, AtomKind::Fuzzy));
    }
    if !neg && !body.is_empty() && body.len() <= 9 && body.bytes().all(|b| b.is_ascii_digit()) {
        return Token::Number(body.parse().unwrap_or(0), atom(body, AtomKind::Fuzzy));
    }
    if let Some((prefix, value)) = body.split_once(':') {
        let p = prefix.to_ascii_lowercase();
        let value = value.trim_matches('"');
        let v = value.to_lowercase();
        let invalid = || Token::Invalid;
        if PREFIXES.contains(&format!("{p}:").as_str()) && value.is_empty() {
            // `-ci:` has no negated form; draw it red instead of as a pending filter.
            return if neg && !negatable(&format!("{p}:")) { invalid() } else { Token::Pending };
        }
        match p.as_str() {
            "repo" => return Token::Repo(v, neg),
            "author" => {
                let login = match (v.as_str(), me) {
                    ("@me", Some(me)) => me.to_lowercase(),
                    _ => v.trim_start_matches('@').to_string(),
                };
                return Token::Author(login, neg);
            }
            "label" => return Token::Label(v, neg),
            "head" | "branch" if !neg => return Token::Head(v),
            "is" => {
                let f = match v.as_str() {
                    "draft" => IsFlag::Draft,
                    "open" => IsFlag::Open,
                    "closed" => IsFlag::Closed,
                    "merged" => IsFlag::Merged,
                    _ => return invalid(),
                };
                return Token::Is(f, neg);
            }
            "ci" if !neg => {
                return match v.as_str() {
                    "failing" | "failure" | "failed" => Token::Ci(CiFlag::Failing),
                    "passing" | "success" | "passed" => Token::Ci(CiFlag::Passing),
                    "pending" => Token::Ci(CiFlag::Pending),
                    "none" => Token::Ci(CiFlag::None),
                    _ => invalid(),
                };
            }
            "review" if !neg => {
                return match v.as_str() {
                    "approved" => Token::Review(ReviewFlag::Approved),
                    "changes" | "changes_requested" => Token::Review(ReviewFlag::Changes),
                    "required" => Token::Review(ReviewFlag::Required),
                    "none" => Token::Review(ReviewFlag::None),
                    _ => invalid(),
                };
            }
            // `-head:`, `-ci:`, `-review:` and any other negated prefix with no meaning.
            _ if neg => return invalid(),
            _ => {}
        }
    }
    // Unknown prefixes (`foo:bar`) are plain text, so titles with colons still match.
    Token::Term(atom(word, AtomKind::Fuzzy), lower(word))
}

/// Split `raw` into tokens: whitespace-separated, `"quoted phrases"` (and `label:"two words"`) kept together.
pub fn parse(raw: &str, me: Option<&str>) -> Query {
    let mut q = Query { raw: raw.to_string(), ..Query::default() };
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let (token, closed) = if b[i] == b'"' {
            let end = raw[i + 1..].find('"').map(|e| i + 1 + e);
            let inner = &raw[i + 1..end.unwrap_or(raw.len())];
            i = end.map_or(raw.len(), |e| e + 1);
            let t = inner.trim();
            (if t.is_empty() { None } else { Some(Token::Phrase(atom(t, AtomKind::Substring), lower(t))) }, end.is_some())
        } else {
            let mut quoted = false;
            while i < b.len() && (quoted || !b[i].is_ascii_whitespace()) {
                if b[i] == b'"' {
                    quoted = !quoted;
                }
                i += 1;
            }
            let word = &raw[start..i];
            (if word == "#" || word == "-" { None } else { Some(classify(word, me)) }, false)
        };
        if let Some(t) = token {
            if closed || i < b.len() {
                q.complete += 1;
            }
            q.tokens.push(t);
            q.spans.push(start..i);
        }
    }
    let lead = raw.trim_start().chars().next();
    q.number_mode = matches!(lead, Some('#' | '0'..='9')) && q.tokens.len() <= 1 && q.tokens.iter().all(|t| matches!(t, Token::Number(..)));
    q
}

/// The GitHub search string for the typed query: complete words (all of them when `all_tokens`)
/// plus the filters mapped to qualifiers, then `base` (the list's qualifiers). `None` when there is
/// nothing GitHub could search for. `repos` resolves `repo:atlas` to a loaded `owner/name`.
pub fn remote_query(q: &Query, base: &str, all_tokens: bool, repos: &[String]) -> Option<String> {
    if q.number_mode {
        return None;
    }
    let n = if all_tokens { q.tokens.len() } else { q.complete };
    let mut words: Vec<String> = Vec::new();
    let mut quals: Vec<String> = Vec::new();
    let minus = |neg: bool| if neg { "-" } else { "" };
    for t in &q.tokens[..n] {
        match t {
            Token::Term(_, w) => words.push(w.clone()),
            Token::Phrase(_, p) => words.push(format!("\"{p}\"")),
            Token::Repo(r, neg) => {
                let found = repos.iter().find(|x| x.eq_ignore_ascii_case(r)).or_else(|| {
                    repos.iter().find(|x| x.to_lowercase().split_once('/').is_some_and(|(_, name)| name == r))
                });
                let found = found.or_else(|| repos.iter().find(|x| x.to_lowercase().contains(r.as_str())));
                match found {
                    Some(nwo) => quals.push(format!("{}repo:{nwo}", minus(*neg))),
                    None if r.contains('/') => quals.push(format!("{}repo:{r}", minus(*neg))),
                    // `repo:atlas` with nothing loaded is not a GitHub qualifier.
                    // Returned rows are still dropped unless the owner/name matches.
                    None => {}
                }
            }
            Token::Author(a, neg) => quals.push(format!("{}author:{a}", minus(*neg))),
            Token::Label(l, neg) if l.contains(' ') => quals.push(format!("{}label:\"{l}\"", minus(*neg))),
            Token::Label(l, neg) => quals.push(format!("{}label:{l}", minus(*neg))),
            Token::Head(h) => quals.push(format!("head:{h}")),
            Token::Is(IsFlag::Draft, neg) => quals.push(format!("draft:{}", !neg)),
            Token::Is(f, neg) => {
                let s = match f {
                    IsFlag::Open => "open",
                    IsFlag::Closed => "closed",
                    _ => "merged",
                };
                quals.push(format!("{}is:{s}", minus(*neg)));
            }
            Token::Ci(f) => match f {
                CiFlag::Failing => quals.push("status:failure".into()),
                CiFlag::Passing => quals.push("status:success".into()),
                CiFlag::Pending => quals.push("status:pending".into()),
                // No qualifier for "no checks". `passes` drops rows that have a rollup.
                CiFlag::None => {}
            },
            Token::Review(f) => quals.push(
                match f {
                    ReviewFlag::Approved => "review:approved",
                    ReviewFlag::Changes => "review:changes_requested",
                    ReviewFlag::Required => "review:required",
                    ReviewFlag::None => "review:none",
                }
                .into(),
            ),
            // Numbers as text are junk on GitHub (docs/search.md §6).
            Token::Number(..) | Token::Invalid | Token::Pending => {}
        }
    }
    if words.is_empty() && quals.is_empty() {
        return None;
    }
    let mut parts = words;
    if !parts.is_empty() {
        parts.push("in:title,body".into());
    }
    parts.extend(quals);
    parts.push(base.to_string());
    parts.push("sort:updated-desc".into());
    Some(parts.join(" ").split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Plain text of a markdown/HTML description for matching: no comments, code fences, tags,
/// link/image targets, URLs or markdown punctuation runs; whitespace collapsed; at most `cap` chars.
pub fn strip_body(raw: &str, cap: usize) -> String {
    let mut s = raw.replace("\r\n", "\n");
    // HTML comments.
    while let Some(a) = s.find("<!--") {
        let end = s[a..].find("-->").map_or(s.len(), |e| a + e + 3);
        s.replace_range(a..end, " ");
    }
    // Fenced code blocks.
    let mut out = String::with_capacity(s.len());
    let mut fenced = false;
    for line in s.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if !fenced {
            out.push_str(line);
            out.push('\n');
        }
    }
    // Tags, link targets, URLs and punctuation, char by char.
    let chars: Vec<char> = out.chars().collect();
    let mut text = String::with_capacity(out.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let rest = || chars[i..].iter().take(8).collect::<String>();
        if c == '<' && chars.get(i + 1).is_some_and(|n| n.is_ascii_alphabetic() || *n == '/' || *n == '!') {
            match chars[i..].iter().position(|&x| x == '>') {
                Some(e) => {
                    i += e + 1;
                    text.push(' ');
                    continue;
                }
                None => break,
            }
        }
        if c == ']' && chars.get(i + 1) == Some(&'(') {
            // `[text](target)` / `![alt](src)`: keep the text, drop the target.
            let e = chars[i..].iter().position(|&x| x == ')').map_or(chars.len(), |e| i + e + 1);
            i = e;
            text.push(' ');
            continue;
        }
        let r = rest();
        if r.starts_with("http://") || r.starts_with("https://") {
            while i < chars.len() && !chars[i].is_whitespace() && chars[i] != ')' && chars[i] != '>' {
                i += 1;
            }
            text.push(' ');
            continue;
        }
        if matches!(c, '#' | '*' | '_' | '`' | '>' | '|' | '~' | '=' | '[' | '!') || (c == '-' && chars.get(i + 1) == Some(&'-')) {
            // Markdown punctuation: runs become one space (a lone `-` or `_` inside a word stays).
            let inside = i > 0 && chars[i - 1].is_alphanumeric() && chars.get(i + 1).is_some_and(|n| n.is_alphanumeric());
            if inside && matches!(c, '_' | '-') {
                text.push(c);
                i += 1;
                continue;
            }
            while i < chars.len() && matches!(chars[i], '#' | '*' | '_' | '`' | '>' | '|' | '~' | '=' | '[' | '!' | '-') {
                i += 1;
            }
            text.push(' ');
            continue;
        }
        text.push(c);
        i += 1;
    }
    let mut collapsed = String::with_capacity(text.len());
    let mut n = 0;
    for w in text.split_whitespace() {
        if n > 0 {
            if n + 1 >= cap {
                break;
            }
            collapsed.push(' ');
            n += 1;
        }
        for c in w.chars() {
            if n >= cap {
                break;
            }
            collapsed.push(c);
            n += 1;
        }
        if n >= cap {
            break;
        }
    }
    collapsed
}

/// A PR's searchable fields, built once per PR (and again when its description arrives).
pub struct Doc {
    title: Utf32String,
    repo: Utf32String,
    branch: Utf32String,
    author: Utf32String,
    labels: Utf32String,
    labels_lc: Vec<String>,
    pub number: u64,
    number_str: String,
    /// Stripped description and its `lower()` copy.
    body: Option<(String, String)>,
    pub updated: i64,
}

impl Doc {
    pub fn new(pr: &PrSummary, body: Option<&str>) -> Self {
        let labels: Vec<&str> = pr.labels.iter().map(|l| l.name.as_str()).collect();
        let mut d = Doc {
            title: pr.title.as_str().into(),
            repo: pr.repository.name_with_owner.as_str().into(),
            branch: pr.head_ref_name.as_str().into(),
            author: login_of(&pr.author).into(),
            labels: labels.join(" ").into(),
            labels_lc: labels.iter().map(|l| l.to_lowercase()).collect(),
            number: pr.number,
            number_str: pr.number.to_string(),
            body: None,
            updated: crate::ui::parse_ts(&pr.updated_at).unwrap_or(0),
        };
        if let Some(b) = body {
            d.set_body(b);
        }
        d
    }

    pub fn set_body(&mut self, stripped: &str) {
        self.body = (!stripped.is_empty()).then(|| (stripped.to_string(), lower(stripped)));
    }

    fn field(&self, f: Field) -> &Utf32String {
        match f {
            Field::Title => &self.title,
            Field::Repo => &self.repo,
            Field::Branch => &self.branch,
            Field::Author => &self.author,
            _ => &self.labels,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub score: u32,
    /// Where the strongest term hit.
    pub field: Field,
    /// Char range of the first description hit.
    pub body_hit: Option<(usize, usize)>,
    pub exact_number: bool,
    /// Terms that matched nothing locally (remote rows only; they rank last).
    pub missed: u8,
}

const FIELDS: [(Field, u32); 5] = [(Field::Title, 10), (Field::Repo, 9), (Field::Branch, 7), (Field::Author, 7), (Field::Labels, 6)];

/// Best weighted score of one free token, its field, and the body hit if it had one.
fn term_score(m: &mut Matcher, t: &Token, d: &Doc, number_mode: bool) -> Option<(u32, Field, Option<(usize, usize)>)> {
    let (a, lc) = match t {
        Token::Number(n, a) => {
            if *n == d.number {
                return Some((100_000, Field::Number, None));
            }
            if d.number_str.starts_with(&n.to_string()) {
                return Some((50_000, Field::Number, None));
            }
            if number_mode {
                return None;
            }
            (a, None)
        }
        Token::Term(a, lc) | Token::Phrase(a, lc) => (a, Some(lc)),
        _ => return None,
    };
    let mut best: Option<(u32, Field, Option<(usize, usize)>)> = None;
    let floor = fuzzy_floor(a);
    for (f, w) in FIELDS {
        if let Some(s) = a.score(d.field(f).slice(..), m).filter(|s| *s >= floor) {
            let s = s as u32 * w;
            if best.is_none_or(|b| s > b.0) {
                best = Some((s, f, None));
            }
        }
    }
    // Bodies: substring on the lowercased copy; short terms skip them (too noisy).
    if let (Some(lc), Some((_, body_lc))) = (lc, &d.body) {
        let len = lc.chars().count();
        if len >= 3
            && let Some(at) = body_lc.find(lc.as_str())
        {
            let s = len as u32 * 16 * 5;
            let start = body_lc[..at].chars().count();
            let hit = Some((start, start + len));
            match &mut best {
                Some(b) if b.0 >= s => {
                    if b.2.is_none() {
                        b.2 = hit;
                    }
                }
                _ => best = Some((s, Field::Body, hit)),
            }
        }
    }
    best
}

/// Least fuzzy score a field hit needs (measured on real titles): letters scattered across a
/// long title ("atlas" in "summaries at three lengths and…": 98, "skin": 64) fall below it, while
/// contiguous words (≈ 28 per letter) and abbreviations ("ghprm" for gh-pr-manager: 125,
/// "frmatls" for form-atlas: 149, "ambnt": 120) stay above.
fn fuzzy_floor(a: &Atom) -> u16 {
    let n = a.needle_text().len() as u16;
    if a.kind != AtomKind::Fuzzy || n < 3 { 0 } else { 20 * n + 4 }
}

fn fuzzy(m: &mut Matcher, needle: &str, hay: &Utf32String) -> bool {
    atom(needle, AtomKind::Fuzzy).score(hay.slice(..), m).is_some()
}

/// Whether the filters let `pr` through.
///
/// `lenient` is for a row GitHub already returned. A positive `label:` is then trusted when
/// the summary's label list is full (`labels(first:10)` in `pr_fields`): the matching name
/// may sit past that page, and the search qualifier already required it.
fn passes(m: &mut Matcher, q: &Query, d: &Doc, pr: &PrSummary, lenient: bool) -> bool {
    let trust_label = lenient && pr.labels.nodes.len() >= 10;
    q.tokens.iter().all(|t| match t {
        Token::Repo(r, neg) => fuzzy(m, r, &d.repo) != *neg,
        Token::Author(a, neg) => fuzzy(m, a, &d.author) != *neg,
        Token::Label(_, false) if trust_label => true,
        Token::Label(l, neg) => d.labels_lc.iter().any(|x| x.contains(l.as_str())) != *neg,
        Token::Head(h) => fuzzy(m, h, &d.branch),
        Token::Is(f, neg) => {
            let yes = match f {
                IsFlag::Draft => pr.is_draft,
                IsFlag::Open => pr.state == "OPEN",
                IsFlag::Closed => pr.state == "CLOSED",
                IsFlag::Merged => pr.state == "MERGED",
            };
            yes != *neg
        }
        Token::Ci(f) => {
            let s = pr.ci_state();
            match f {
                CiFlag::Failing => matches!(s, Some("FAILURE" | "ERROR")),
                CiFlag::Passing => s == Some("SUCCESS"),
                CiFlag::Pending => matches!(s, Some("PENDING" | "EXPECTED")),
                CiFlag::None => s.is_none(),
            }
        }
        Token::Review(f) => {
            let r = pr.review_decision.as_deref();
            match f {
                ReviewFlag::Approved => r == Some("APPROVED"),
                ReviewFlag::Changes => r == Some("CHANGES_REQUESTED"),
                ReviewFlag::Required => r == Some("REVIEW_REQUIRED"),
                ReviewFlag::None => r.is_none(),
            }
        }
        _ => true,
    })
}

/// Score one PR: every free term must hit some field (AND), summed over terms; filters gate.
/// `lenient` (rows GitHub returned) keeps a PR whose terms miss locally, counting the misses.
/// Filters still apply: a query GitHub cannot express (`ci:none`, an unresolved `repo:`)
/// must not show rows that contradict it.
pub fn score(m: &mut Matcher, q: &Query, d: &Doc, pr: &PrSummary, lenient: bool) -> Option<Match> {
    if !passes(m, q, d, pr, lenient) {
        return None;
    }
    let mut out = Match { score: 0, field: Field::Title, body_hit: None, exact_number: false, missed: 0 };
    let mut top = 0;
    for t in q.tokens.iter().filter(|t| t.is_free()) {
        match term_score(m, t, d, q.number_mode) {
            Some((s, f, hit)) => {
                out.score += s;
                if s > top {
                    top = s;
                    out.field = f;
                }
                if out.body_hit.is_none() {
                    out.body_hit = hit;
                }
                out.exact_number |= s == 100_000 && f == Field::Number;
            }
            None if lenient => out.missed = out.missed.saturating_add(1),
            None => return None,
        }
    }
    Some(out)
}

/// Order: fewer misses, higher score, more recently updated, original position.
pub fn order(a: (&Match, i64, usize), b: (&Match, i64, usize)) -> std::cmp::Ordering {
    a.0.missed.cmp(&b.0.missed).then(b.0.score.cmp(&a.0.score)).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2))
}

/// Matching PRs (indices into `prs`/`docs`), best first. An empty query keeps the list order.
pub fn rank(m: &mut Matcher, q: &Query, docs: &[Doc], prs: &[PrSummary]) -> Vec<(usize, Match)> {
    let blank = Match { score: 0, field: Field::Title, body_hit: None, exact_number: false, missed: 0 };
    if q.is_empty() {
        return (0..prs.len().min(docs.len())).map(|i| (i, blank)).collect();
    }
    let mut out: Vec<(usize, Match)> = docs.iter().zip(prs).enumerate().filter_map(|(i, (d, p))| score(m, q, d, p, false).map(|s| (i, s))).collect();
    out.sort_by(|a, b| order((&a.1, docs[a.0].updated, a.0), (&b.1, docs[b.0].updated, b.0)));
    out
}

/// Char indices to highlight in `field` (title, repo, branch, author or labels) of a drawn row.
/// A term lights up only the fields where it scored close to its best, so a strong repo hit
/// doesn't also sprinkle stray letters over the title.
pub fn highlight(m: &mut Matcher, q: &Query, field: Field, d: &Doc) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    if field == Field::Number {
        for t in &q.tokens {
            if let Token::Number(n, _) = t {
                let s = n.to_string();
                if d.number_str.starts_with(&s) {
                    out.extend(0..s.len() as u32);
                }
            }
        }
        return out;
    }
    let hay = d.field(field);
    for t in &q.tokens {
        let a = match t {
            Token::Term(a, _) | Token::Phrase(a, _) => a.clone(),
            Token::Number(n, a) if !q.number_mode && !d.number_str.starts_with(&n.to_string()) => a.clone(),
            Token::Repo(r, false) if field == Field::Repo => atom(r, AtomKind::Fuzzy),
            Token::Author(r, false) if field == Field::Author => atom(r, AtomKind::Fuzzy),
            Token::Head(r) if field == Field::Branch => atom(r, AtomKind::Fuzzy),
            Token::Label(r, false) if field == Field::Labels => atom(r, AtomKind::Substring),
            _ => continue,
        };
        if t.is_free() {
            let floor = fuzzy_floor(&a);
            let weight = |f: Field| FIELDS.iter().find(|(x, _)| *x == f).map_or(1, |x| x.1);
            let mine = a.score(hay.slice(..), m).filter(|s| *s >= floor).map_or(0, |s| s as u32 * weight(field));
            let best = FIELDS.iter().filter_map(|(f, w)| a.score(d.field(*f).slice(..), m).filter(|s| *s >= floor).map(|s| s as u32 * w)).max().unwrap_or(0);
            if mine == 0 || mine * 5 < best * 4 {
                continue;
            }
        }
        // Every literal occurrence (nucleo marks only its best one, often past the cut);
        // abbreviations fall back to the fuzzy positions.
        let lit = match t {
            Token::Term(_, w) | Token::Phrase(_, w) => Some(w.clone()),
            Token::Number(n, _) => Some(n.to_string()),
            Token::Repo(w, _) | Token::Author(w, _) | Token::Head(w) | Token::Label(w, _) => Some(w.clone()),
            _ => None,
        };
        let text = lower(&hay.to_string());
        let before = out.len();
        if let Some(w) = lit.filter(|w| !w.is_empty()) {
            let n = w.chars().count() as u32;
            for (at, _) in text.match_indices(w.as_str()) {
                let s = text[..at].chars().count() as u32;
                out.extend(s..s + n);
            }
        }
        if out.len() == before {
            a.indices(hay.slice(..), m, &mut out);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// About `width` chars of the description around the first hit, cut at word boundaries with `…`
/// on cut sides, and the char ranges of every hit inside it.
pub fn snippet(d: &Doc, q: &Query, width: usize) -> Option<(String, Vec<(usize, usize)>)> {
    let (body, lc) = d.body.as_ref()?;
    let mut hits: Vec<(usize, usize)> = Vec::new();
    for t in &q.tokens {
        let needle = match t {
            Token::Term(_, w) | Token::Phrase(_, w) if w.chars().count() >= 3 => w,
            _ => continue,
        };
        let n = needle.chars().count();
        for (at, _) in lc.match_indices(needle.as_str()) {
            let s = lc[..at].chars().count();
            hits.push((s, s + n));
        }
    }
    let first = *hits.iter().min()?;
    let chars: Vec<char> = body.chars().collect();
    let width = width.max(12);
    let lead = width.saturating_sub(first.1 - first.0) / 3;
    let mut start = first.0.saturating_sub(lead);
    let mut end = (start + width).min(chars.len());
    if end - start < width {
        start = end.saturating_sub(width);
    }
    // Word boundaries (a little slack either way).
    if start > 0
        && let Some(sp) = chars[start..first.0].iter().position(|c| c.is_whitespace())
    {
        start += sp + 1;
    }
    if end < chars.len()
        && let Some(sp) = chars[first.1.min(end)..end].iter().rposition(|c| c.is_whitespace())
    {
        end = first.1.min(end) + sp;
    }
    let mut text = String::new();
    let shift = if start > 0 {
        text.push('…');
        1
    } else {
        0
    };
    text.extend(&chars[start..end]);
    if end < chars.len() {
        text.push('…');
    }
    let mut ranges: Vec<(usize, usize)> = hits
        .into_iter()
        .filter(|&(a, b)| a >= start && b <= end)
        .map(|(a, b)| (a - start + shift, b - start + shift))
        .collect();
    ranges.sort_unstable();
    Some((text, ranges))
}

/// Values in view that `Tab` can complete filter tokens from.
#[derive(Default)]
pub struct Values {
    pub repos: Vec<String>,
    pub authors: Vec<String>,
    pub labels: Vec<String>,
    pub branches: Vec<String>,
}

/// Completions for the token being typed (the last one, when the input doesn't end in a space).
pub fn complete(q: &Query, values: &Values) -> Vec<String> {
    let Some(span) = q.spans.last().filter(|s| s.end == q.raw.len()) else { return Vec::new() };
    let word = &q.raw[span.clone()];
    let (neg, body) = match word.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, word),
    };
    let minus = if neg { "-" } else { "" };
    let Some((prefix, value)) = body.split_once(':') else {
        let w = body.to_ascii_lowercase();
        return PREFIXES.iter().filter(|p| !w.is_empty() && p.starts_with(&w) && **p != w && (!neg || negatable(p))).map(|p| format!("{minus}{p}")).collect();
    };
    if neg && !negatable(&format!("{}:", prefix.to_ascii_lowercase())) {
        return Vec::new();
    }
    let v = value.trim_matches('"').to_lowercase();
    let list: Vec<String> = match prefix.to_ascii_lowercase().as_str() {
        "repo" => values.repos.clone(),
        "author" => values.authors.clone(),
        "label" => values.labels.clone(),
        "head" | "branch" => values.branches.clone(),
        "is" => ["draft", "open", "closed", "merged"].map(String::from).to_vec(),
        "ci" => ["failing", "passing", "pending", "none"].map(String::from).to_vec(),
        "review" => ["approved", "changes", "required", "none"].map(String::from).to_vec(),
        _ => return Vec::new(),
    };
    let fits = |c: &String| {
        let l = c.to_lowercase();
        l.starts_with(&v) || l.split_once('/').is_some_and(|(_, name)| name.starts_with(&v))
    };
    let mut out: Vec<String> = Vec::new();
    for c in list.iter().filter(|c| fits(c)) {
        let quoted = if c.contains(' ') { format!("\"{c}\"") } else { c.clone() };
        let t = format!("{minus}{prefix}:{quoted}");
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Score of `hay` against the query's free words (AND), for repos, scopes and commands.
pub fn score_text(m: &mut Matcher, q: &Query, hay: &str) -> Option<u32> {
    let hay: Utf32String = hay.into();
    let mut total = 0u32;
    let mut any = false;
    for t in &q.tokens {
        let a = match t {
            Token::Term(a, _) | Token::Phrase(a, _) => a,
            _ => continue,
        };
        total += a.score(hay.slice(..), m).filter(|s| *s >= fuzzy_floor(a))? as u32;
        any = true;
    }
    any.then_some(total)
}

/// Char indices of the query's free words in `hay`.
pub fn text_indices(m: &mut Matcher, q: &Query, hay: &str) -> Vec<u32> {
    let hay: Utf32String = hay.into();
    let mut out = Vec::new();
    for t in &q.tokens {
        if let Token::Term(a, _) | Token::Phrase(a, _) = t {
            a.indices(hay.slice(..), m, &mut out);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Things the palette can run on the selected PR (or globally).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Comment,
    Merge,
    Approve,
    RequestChanges,
    CloseOrReopen,
    Browser,
    CopyCheckout,
    CopyUrl,
    Refresh,
    Density,
    StateOpen,
    StateClosed,
    StateMerged,
    StateAll,
    OnlyRepo,
    Images,
    UnreadOnly,
    MarkAllRead,
    UnreadFirst,
    NextNew,
    StackView,
    StackPrev,
    StackNext,
    StackUpdate,
    StackLink,
    StackMerge,
    StackBrowser,
    GroupStacks,
    Help,
    Quit,
}

impl Command {
    pub const ALL: [Command; 30] = [
        Command::Comment,
        Command::Merge,
        Command::Approve,
        Command::RequestChanges,
        Command::CloseOrReopen,
        Command::Browser,
        Command::CopyCheckout,
        Command::CopyUrl,
        Command::Refresh,
        Command::Density,
        Command::StateOpen,
        Command::StateClosed,
        Command::StateMerged,
        Command::StateAll,
        Command::OnlyRepo,
        Command::Images,
        Command::UnreadOnly,
        Command::MarkAllRead,
        Command::UnreadFirst,
        Command::NextNew,
        Command::StackView,
        Command::StackPrev,
        Command::StackNext,
        Command::StackUpdate,
        Command::StackLink,
        Command::StackMerge,
        Command::StackBrowser,
        Command::GroupStacks,
        Command::Help,
        Command::Quit,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Command::Comment => "Comment…",
            Command::Merge => "Merge…",
            Command::Approve => "Approve",
            Command::RequestChanges => "Request changes…",
            Command::CloseOrReopen => "Close or reopen pull request",
            Command::Browser => "Open in browser",
            Command::CopyCheckout => "Copy `gh pr checkout` command",
            Command::CopyUrl => "Copy PR URL",
            Command::Refresh => "Refresh",
            Command::Density => "Cycle display density",
            Command::StateOpen => "State: Open",
            Command::StateClosed => "State: Closed",
            Command::StateMerged => "State: Merged",
            Command::StateAll => "State: All",
            Command::OnlyRepo => "Only this repository",
            Command::Images => "Show / hide images",
            Command::UnreadOnly => "Unread only (show / hide read PRs)",
            Command::MarkAllRead => "Mark all read",
            Command::UnreadFirst => "Sort unread first (on / off)",
            Command::NextNew => "Next new entry (NEW since you last looked)",
            Command::StackView => "Stack: view",
            Command::StackPrev => "Stack: previous PR",
            Command::StackNext => "Stack: next PR",
            Command::StackUpdate => "Stack: update branch onto parent",
            Command::StackLink => "Stack: link into a GitHub stack",
            Command::StackMerge => "Stack: merge up to here…",
            Command::StackBrowser => "Stack: open on GitHub",
            Command::GroupStacks => "Group by stack (on / off)",
            Command::Help => "Help",
            Command::Quit => "Quit",
        }
    }

    /// The key that does the same outside the palette.
    pub fn key(self) -> &'static str {
        match self {
            Command::Comment => "c",
            Command::Merge => "m",
            Command::Approve => "a",
            Command::RequestChanges => "R",
            Command::CloseOrReopen => "x",
            Command::Browser => "o",
            Command::CopyCheckout => "y",
            Command::CopyUrl => "",
            Command::Refresh => "r",
            Command::Density => "D",
            Command::StateOpen | Command::StateClosed | Command::StateMerged | Command::StateAll => "f",
            Command::OnlyRepo => "e",
            Command::Images => "I",
            Command::UnreadOnly => "u",
            Command::MarkAllRead => "U",
            Command::UnreadFirst => "",
            Command::NextNew => "n",
            Command::StackView => "8",
            Command::StackPrev => "<",
            Command::StackNext => ">",
            Command::StackUpdate => "b",
            Command::StackLink => "L",
            Command::StackMerge => "M",
            Command::StackBrowser | Command::GroupStacks => "",
            Command::Help => "?",
            Command::Quit => "q",
        }
    }

    /// Acts on the selected PR.
    pub fn needs_pr(self) -> bool {
        matches!(
            self,
            Command::Comment
                | Command::Merge
                | Command::Approve
                | Command::RequestChanges
                | Command::CloseOrReopen
                | Command::Browser
                | Command::CopyCheckout
                | Command::CopyUrl
                | Command::OnlyRepo
                | Command::StackView
                | Command::StackPrev
                | Command::StackNext
                | Command::StackUpdate
                | Command::StackLink
                | Command::StackMerge
                | Command::StackBrowser
        )
    }
}

/// Commands whose label matches the free words, best first.
pub fn commands(q: &Query, m: &mut Matcher) -> Vec<(u16, Command)> {
    if q.number_mode || !q.has_free() {
        return Vec::new();
    }
    let mut out: Vec<(u16, Command)> =
        Command::ALL.iter().filter_map(|&c| score_text(m, q, c.label()).map(|s| (s.min(u16::MAX as u32) as u16, c))).collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out
}

/// A token bucket: `cap` searches, refilled one per `every`.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    tokens: f64,
    cap: f64,
    every: std::time::Duration,
    at: std::time::Instant,
}

impl TokenBucket {
    pub fn new(cap: u32, every: std::time::Duration, now: std::time::Instant) -> Self {
        Self { tokens: cap as f64, cap: cap as f64, every, at: now }
    }

    fn refill(&mut self, now: std::time::Instant) {
        let gained = now.saturating_duration_since(self.at).as_secs_f64() / self.every.as_secs_f64();
        self.tokens = (self.tokens + gained).min(self.cap);
        self.at = now;
    }

    /// Spend one token if there is one.
    pub fn take(&mut self, now: std::time::Instant) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// How long until a token is available.
    pub fn wait(&mut self, now: std::time::Instant) -> std::time::Duration {
        self.refill(now);
        if self.tokens >= 1.0 { std::time::Duration::ZERO } else { self.every.mul_f64(1.0 - self.tokens) }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::github::{CommitNode, Count, LabelName, LastCommit, Login, Nodes, RepoRef, RollupOnly, RollupState};
    use std::time::{Duration, Instant};

    pub fn pr(number: u64, repo: &str, title: &str, updated: &str) -> PrSummary {
        PrSummary {
            id: format!("{repo}#{number}"),
            number,
            title: title.into(),
            url: format!("https://github.com/{repo}/pull/{number}"),
            is_draft: false,
            state: "OPEN".into(),
            updated_at: updated.into(),
            author: Some(Login { login: "wiiiimm".into() }),
            repository: RepoRef { name_with_owner: repo.into() },
            review_decision: None,
            comments: Count { total_count: 0 },
            commits: LastCommit::default(),
            head_ref_name: "feat/x".into(),
            labels: Nodes { nodes: vec![Some(LabelName { name: "deps".into() })] },
            reviews: Count::default(),
            review_threads: Count::default(),
            review_requests: Count::default(),
            head_ref_oid: String::new(),
            mergeable: None,
            last_edited_at: None,
            is_read_by_viewer: None,
            viewer_latest_review_request: None,
            base_ref_name: "main".into(),
            is_cross_repository: false,
            stack: None,
            stack_entry: None,
        }
    }

    fn ranked(q: &str, prs: &[PrSummary], bodies: &[Option<&str>]) -> Vec<u64> {
        let docs: Vec<Doc> = prs.iter().zip(bodies.iter().chain(std::iter::repeat(&None))).map(|(p, b)| Doc::new(p, *b)).collect();
        let q = parse(q, Some("wiiiimm"));
        rank(&mut matcher(), &q, &docs, prs).into_iter().map(|(i, _)| prs[i].number).collect()
    }

    #[test]
    fn parses_tokens() {
        let q = parse(r#"fix "ambient occl" -label:wip author:@me foo:bar is:foo repo:"#, Some("Me"));
        assert!(matches!(&q.tokens[0], Token::Term(_, w) if w == "fix"));
        assert!(matches!(&q.tokens[1], Token::Phrase(_, p) if p == "ambient occl"));
        assert!(matches!(&q.tokens[2], Token::Label(l, true) if l == "wip"));
        assert!(matches!(&q.tokens[3], Token::Author(a, false) if a == "me"));
        assert!(matches!(&q.tokens[4], Token::Term(_, w) if w == "foo:bar"));
        assert!(matches!(&q.tokens[5], Token::Invalid));
        assert!(matches!(&q.tokens[6], Token::Pending));
        assert_eq!(q.complete, 6);
        assert_eq!(&q.raw[q.spans[1].clone()], "\"ambient occl\"");
        assert!(!q.number_mode);
        let q = parse(r#"label:"needs review" x"#, None);
        assert!(matches!(&q.tokens[0], Token::Label(l, false) if l == "needs review"));
    }

    #[test]
    fn number_mode() {
        for s in ["#14", "14", "14 ", " #14 "] {
            let q = parse(s, None);
            assert!(q.number_mode, "{s}");
            assert_eq!(q.number(), Some(14), "{s}");
        }
        assert!(parse("#", None).number_mode);
        assert!(!parse("14 fix", None).number_mode);
        assert!(!parse("fix 14", None).number_mode);
        assert_eq!(parse("14 ", None).complete, 1);
        assert_eq!(parse("14", None).complete, 0);
    }

    #[test]
    fn and_semantics() {
        let prs = [pr(1, "a/form-atlas", "skin deforms", "2026-01-01T00:00:00Z"), pr(2, "a/opscope", "skin colour", "2026-01-02T00:00:00Z")];
        assert_eq!(ranked("atlas skin", &prs, &[]), vec![1]);
        assert_eq!(ranked("skin", &prs, &[]), vec![2, 1]);
        assert!(ranked("skin zzqx", &prs, &[]).is_empty());
    }

    #[test]
    fn field_precedence() {
        let prs = [
            pr(12, "a/b", "fix 439 things", "2026-01-03T00:00:00Z"),
            pr(439, "a/b", "fix a bug", "2026-01-01T00:00:00Z"),
        ];
        assert_eq!(ranked("fix 439", &prs, &[]), vec![439, 12]);
        // Title beats repo beats body.
        let prs = [
            pr(1, "a/plain", "unrelated", "2026-01-03T00:00:00Z"),
            pr(2, "a/gizmo", "unrelated", "2026-01-02T00:00:00Z"),
            pr(3, "a/plain", "gizmo work", "2026-01-01T00:00:00Z"),
        ];
        assert_eq!(ranked("gizmo", &prs, &[Some("this mentions gizmo in the body"), None, None]), vec![3, 2, 1]);
    }

    #[test]
    fn number_ranking() {
        let prs = [
            pr(140, "a/b", "x", "2026-01-05T00:00:00Z"),
            pr(14, "a/c", "y", "2026-01-01T00:00:00Z"),
            pr(142, "a/b", "z", "2026-01-04T00:00:00Z"),
            pr(14, "a/b", "w", "2026-01-02T00:00:00Z"),
            pr(41, "a/b", "has 14 in title", "2026-01-09T00:00:00Z"),
        ];
        // Exact first (newest first among them), then prefix by recency; no text hits.
        assert_eq!(ranked("#14", &prs, &[]), vec![14, 14, 140, 142]);
        let r = ranked("14", &prs, &[]);
        assert_eq!(r, vec![14, 14, 140, 142]);
        let docs: Vec<Doc> = prs.iter().map(|p| Doc::new(p, None)).collect();
        let res = rank(&mut matcher(), &parse("14", None), &docs, &prs);
        assert_eq!(prs[res[0].0].repository.name_with_owner, "a/b");
        assert!(res[0].1.exact_number && !res[2].1.exact_number);
    }

    #[test]
    fn scattered_letters_dont_match() {
        let prs = [pr(1, "a/b", "summaries at three lengths and key points with pull-quotes", "2026-01-02T00:00:00Z"), pr(2, "a/b", "SWR-883: ambient occlusion", "2026-01-01T00:00:00Z")];
        assert!(ranked("skin", &prs, &[]).is_empty());
        assert!(ranked("atlas", &prs, &[]).is_empty());
        assert_eq!(ranked("ambnt", &prs, &[]), vec![2]);
        assert_eq!(ranked("swr", &prs, &[]), vec![2]);
    }

    #[test]
    fn tie_break_by_updated() {
        let prs = [pr(1, "a/b", "same title", "2026-01-01T00:00:00Z"), pr(2, "a/b", "same title", "2026-02-01T00:00:00Z")];
        assert_eq!(ranked("same", &prs, &[]), vec![2, 1]);
    }

    #[test]
    fn filters_gate() {
        let mut a = pr(1, "a/form-atlas", "one", "2026-01-01T00:00:00Z");
        a.is_draft = true;
        let mut b = pr(2, "a/opscope", "two", "2026-01-02T00:00:00Z");
        b.labels = Nodes::default();
        b.state = "MERGED".into();
        let prs = [a, b];
        assert_eq!(ranked("repo:atlas", &prs, &[]), vec![1]);
        assert_eq!(ranked("-repo:atlas", &prs, &[]), vec![2]);
        assert_eq!(ranked("is:draft", &prs, &[]), vec![1]);
        assert_eq!(ranked("is:merged", &prs, &[]), vec![2]);
        assert_eq!(ranked("label:dep", &prs, &[]), vec![1]);
        assert_eq!(ranked("-label:deps", &prs, &[]), vec![2]);
        assert_eq!(ranked("author:@me", &prs, &[]), vec![2, 1]);
        assert_eq!(ranked("ci:none review:none", &prs, &[]), vec![2, 1]);
        // Invalid filters are ignored, not fatal.
        assert_eq!(ranked("is:foo one", &prs, &[]), vec![1]);
        assert_eq!(ranked("-ci:failing one", &prs, &[]), vec![1]);
    }

    #[test]
    fn negated_unsupported_prefixes_are_invalid() {
        for s in ["-head:feat", "-branch:feat", "-ci:failing", "-ci:", "-review:approved"] {
            assert!(matches!(parse(s, None).tokens[0], Token::Invalid), "{s}");
        }
        assert!(matches!(parse("head:feat", None).tokens[0], Token::Head(_)));
        assert!(matches!(parse("ci:failing", None).tokens[0], Token::Ci(CiFlag::Failing)));
        assert!(matches!(parse("-repo:atlas", None).tokens[0], Token::Repo(_, true)));
        assert!(matches!(parse("-is:draft", None).tokens[0], Token::Is(IsFlag::Draft, true)));
        let v = Values::default();
        assert!(complete(&parse("-c", None), &v).is_empty());
        assert!(complete(&parse("-ci:f", None), &v).is_empty());
        assert_eq!(complete(&parse("-r", None), &v), vec!["-repo:"]);
        assert_eq!(complete(&parse("-is:d", None), &v), vec!["-is:draft"]);
    }

    #[test]
    fn short_terms_skip_bodies() {
        let prs = [pr(1, "a/b", "nothing", "2026-01-01T00:00:00Z")];
        assert!(ranked("zq", &prs, &[Some("zq zq zq")]).is_empty());
        assert_eq!(ranked("zqx", &prs, &[Some("has zqx inside")]), vec![1]);
        assert_eq!(ranked("\"has zqx\"", &prs, &[Some("has zqx inside")]), vec![1]);
    }

    #[test]
    fn lenient_keeps_remote_rows() {
        let p = pr(1, "a/b", "unrelated", "2026-01-01T00:00:00Z");
        let d = Doc::new(&p, None);
        let q = parse("shoulder girdle", None);
        let mut m = matcher();
        assert!(score(&mut m, &q, &d, &p, false).is_none());
        let s = score(&mut m, &q, &d, &p, true).unwrap();
        assert_eq!(s.missed, 2);
        let hit = score(&mut m, &parse("unrelated", None), &d, &p, true).unwrap();
        assert_eq!(order((&hit, 0, 1), (&s, 0, 0)), std::cmp::Ordering::Less);
    }

    #[test]
    fn lenient_still_applies_filters() {
        let mut failing = pr(1, "a/b", "unrelated", "2026-01-01T00:00:00Z");
        failing.commits = LastCommit {
            nodes: vec![Some(CommitNode { commit: RollupOnly { status_check_rollup: Some(RollupState { state: "FAILURE".into() }) } })],
            ..LastCommit::default()
        };
        let other = pr(2, "a/other", "unrelated", "2026-01-02T00:00:00Z");
        let d_fail = Doc::new(&failing, None);
        let d_other = Doc::new(&other, None);
        let mut m = matcher();
        let q = parse("shoulder ci:none", None);
        assert!(score(&mut m, &q, &d_fail, &failing, true).is_none());
        assert_eq!(score(&mut m, &q, &d_other, &other, true).unwrap().missed, 1);
        let q = parse("shoulder repo:other", None);
        assert!(score(&mut m, &q, &d_fail, &failing, true).is_none());
        assert!(score(&mut m, &q, &d_other, &other, true).is_some());
        // Fewer than 10 labels: a missing label is a real miss, on GitHub rows too.
        assert!(score(&mut m, &parse("label:nope", None), &d_other, &other, true).is_none());
        // A full label page can hide the name GitHub already matched.
        let mut many = pr(3, "a/b", "shoulder", "2026-01-03T00:00:00Z");
        many.labels = Nodes { nodes: (0..10).map(|i| Some(LabelName { name: format!("l{i}") })).collect() };
        let d_many = Doc::new(&many, None);
        assert!(score(&mut m, &parse("label:hidden", None), &d_many, &many, false).is_none());
        assert!(score(&mut m, &parse("label:hidden", None), &d_many, &many, true).is_some());
        many.commits = failing.commits.clone();
        let d_many = Doc::new(&many, None);
        assert!(score(&mut m, &parse("label:hidden ci:none", None), &d_many, &many, true).is_none());
    }

    #[test]
    fn strips_bodies() {
        let raw = "## Summary\r\n<!-- template -->Fixes the **skin** mesh. See [the docs](https://x.io/a) and https://example.com/z.\n```rust\nlet secret = 1;\n```\n<img src=\"a.png\" alt=\"x\"> ![shot](https://u/s.png) snake_case - item";
        let s = strip_body(raw, 2000);
        assert_eq!(s, "Summary Fixes the skin mesh. See the docs and shot snake_case - item");
        let cut = strip_body("ééé ééé", 5);
        assert_eq!(cut, "ééé é");
        assert_eq!(cut.chars().count(), 5);
    }

    #[test]
    fn snippet_window_and_ranges() {
        let body = "Lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua the skin mesh follows the atlas rig closely";
        let p = pr(1, "a/b", "t", "2026-01-01T00:00:00Z");
        let d = Doc::new(&p, Some(body));
        let (text, ranges) = snippet(&d, &parse("skin atlas", None), 40).unwrap();
        assert!(text.starts_with('…') && text.ends_with('…'), "{text}");
        assert!(text.chars().count() <= 42, "{text}");
        let chars: Vec<char> = text.chars().collect();
        let words: Vec<String> = ranges.iter().map(|&(a, b)| chars[a..b].iter().collect()).collect();
        assert_eq!(words, vec!["skin", "atlas"]);
        assert!(snippet(&d, &parse("zzz", None), 40).is_none());
    }

    #[test]
    fn remote_query_composition() {
        let base = "is:pr archived:false is:open org:acme";
        let repos = vec!["acme/form-atlas".to_string()];
        // Only complete words while typing.
        assert_eq!(remote_query(&parse("shoul", None), base, false, &repos), None);
        assert_eq!(
            remote_query(&parse("shoulder gir", None), base, false, &repos).unwrap(),
            "shoulder in:title,body is:pr archived:false is:open org:acme sort:updated-desc"
        );
        assert_eq!(
            remote_query(&parse("\"skin mesh\" repo:atlas is:draft ci:failing -label:wip", None), base, true, &repos).unwrap(),
            "\"skin mesh\" in:title,body repo:acme/form-atlas draft:true status:failure -label:wip is:pr archived:false is:open org:acme sort:updated-desc"
        );
        assert_eq!(remote_query(&parse("#14", None), base, true, &repos), None);
        assert_eq!(remote_query(&parse("14", None), base, true, &repos), None);
    }

    #[test]
    fn completes_tokens() {
        let v = Values { repos: vec!["acme/form-atlas".into(), "acme/opscope".into()], labels: vec!["needs review".into()], ..Values::default() };
        assert_eq!(complete(&parse("fix rep", None), &v), vec!["repo:"]);
        assert_eq!(complete(&parse("repo:fo", None), &v), vec!["repo:acme/form-atlas"]);
        assert_eq!(complete(&parse("is:d", None), &v), vec!["is:draft"]);
        assert_eq!(complete(&parse("-label:ne", None), &v), vec!["-label:\"needs review\""]);
        assert!(complete(&parse("repo:fo ", None), &v).is_empty());
    }

    #[test]
    fn highlights_best_fields() {
        let p = pr(151, "acme/form-atlas", "app skin deforms", "2026-01-01T00:00:00Z");
        let d = Doc::new(&p, None);
        let q = parse("atlas skin", None);
        let mut m = matcher();
        assert_eq!(highlight(&mut m, &q, Field::Title, &d), vec![4, 5, 6, 7]);
        assert_eq!(highlight(&mut m, &q, Field::Repo, &d), vec![10, 11, 12, 13, 14]);
        assert_eq!(highlight(&mut m, &parse("#15", None), Field::Number, &d), vec![0, 1]);
        // Every occurrence, not just nucleo's best one.
        let p = pr(145, "acme/form-atlas", "body skin detail, matched to her face skin", "2026-01-01T00:00:00Z");
        let d = Doc::new(&p, None);
        assert_eq!(highlight(&mut m, &parse("skin", None), Field::Title, &d), vec![5, 6, 7, 8, 38, 39, 40, 41]);
    }

    #[test]
    fn commands_match_labels() {
        let mut m = matcher();
        let c = commands(&parse("merge", None), &mut m);
        assert_eq!(c[0].1, Command::Merge);
        assert!(commands(&parse("#14", None), &mut m).is_empty());
        assert!(commands(&parse("is:draft", None), &mut m).is_empty());
    }

    #[test]
    fn token_bucket_refills_and_waits() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(20, Duration::from_secs(3), t0);
        for _ in 0..20 {
            assert!(b.take(t0));
        }
        assert!(!b.take(t0));
        assert_eq!(b.wait(t0), Duration::from_secs(3));
        assert!(!b.take(t0 + Duration::from_secs(2)));
        assert!(b.take(t0 + Duration::from_secs(3)));
        // Never more than the cap.
        assert!(b.wait(t0 + Duration::from_secs(600)).is_zero());
        for _ in 0..20 {
            assert!(b.take(t0 + Duration::from_secs(600)));
        }
        assert!(!b.take(t0 + Duration::from_secs(600)));
    }

    /// 300 PRs with ~1.9 KB bodies, like the bench in docs/search.md.
    pub fn fixture() -> (Vec<PrSummary>, Vec<Doc>) {
        let words = ["skin", "atlas", "form", "mesh", "rig", "shoulder", "girdle", "cache", "summary", "tmux", "chart", "pixel", "joint", "deform", "pose"];
        let prs: Vec<PrSummary> = (0..300)
            .map(|i| {
                let title = format!("SWR-{}: {} {} {} for the {}", 800 + i, words[i % 15], words[(i * 7) % 15], words[(i * 3) % 15], words[(i * 11) % 15]);
                pr(i as u64 + 1, &format!("stealth-factory/{}-{}", words[i % 5], words[(i / 5) % 15]), &title, &format!("2026-01-{:02}T00:00:00Z", 1 + i % 28))
            })
            .collect();
        let docs = prs
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let body: String = (0..320).map(|k| words[(i * 13 + k * 7) % 15]).collect::<Vec<_>>().join(" ");
                Doc::new(p, Some(&strip_body(&body, BODY_CAP)))
            })
            .collect();
        (prs, docs)
    }

    #[test]
    fn ranks_300_prs_quickly() {
        let (prs, docs) = fixture();
        let mut m = matcher();
        for q in ["fix", "swr 88", "form atlas skin", "shoulder", "#14"] {
            let q = parse(q, None);
            let t = Instant::now();
            let r = rank(&mut m, &q, &docs, &prs);
            let el = t.elapsed();
            assert!(el < Duration::from_millis(5), "{} took {el:?}", q.raw);
            eprintln!("rank {:<16} {:>4} hits in {el:?}", q.raw, r.len());
        }
    }
}
