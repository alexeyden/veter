//! Hint detection for the overlay's hint mode — the auto-detected
//! counterpart to [`crate::search`]'s typed query.
//!
//! Where search asks the user what to look for, this scans the same
//! [`TextIndex`] for things that are *inherently* actionable — a URL, a
//! file path, a git hash — and hands back [`MatchSpan`]s in exactly the
//! shape search produces. That is the whole point of the module
//! boundary: downstream (label assignment, highlight projection,
//! scroll-to-match, selection) neither knows nor cares which producer
//! filled the list.
//!
//! Two things differ from search and drive the design:
//!
//! * **Spans may cross a soft wrap.** A long URL in a 100-column
//!   terminal is *usually* broken by the right margin, so per-row
//!   matching would hand back two halves and opening either one is
//!   wrong. Rows flagged [`IndexedRow::wrapped`] are joined into one
//!   logical line before scanning and the resulting byte range is
//!   mapped back to a possibly multi-row [`MatchSpan`].
//!
//! * **Boundaries matter more than the pattern.** Terminal text puts
//!   URLs inside parentheses and at the end of sentences, and grep puts
//!   `:42:7` after a filename. The scanners here are hand-written rather
//!   than regex-driven precisely so the tail rules (balanced parens,
//!   sentence punctuation, the `:line:col` suffix) can be expressed
//!   directly; those rules are what decide whether the feature feels
//!   right, and they read poorly as a regex.
//!
//! Detectors are independent: each scans the whole logical line, and
//! overlaps are resolved afterwards by the caller's configured priority
//! (see [`find_hints`]). So a hash-looking path segment inside a URL
//! loses to the URL without either scanner knowing about the other.

use memchr::memmem;

use crate::search::{IndexedRow, MatchSpan, TextIndex};

/// What a hint was recognised as. Reaches a user command as
/// `$VETER_HINT_KIND`, so one binding can branch on url-vs-path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintKind {
    Url,
    Email,
    Uuid,
    Ip,
    Path,
    File,
    Hash,
    Color,
}

impl HintKind {
    /// Every kind, in the default priority order: the more specific and
    /// more strongly delimited a pattern is, the earlier it goes, since
    /// [`find_hints`] resolves overlaps by this order. URL first (it
    /// swallows path- and hash-shaped substrings), hash last (the
    /// loosest pattern of the set).
    pub const DEFAULT_ORDER: &'static [HintKind] = &[
        HintKind::Url,
        HintKind::Email,
        HintKind::Uuid,
        HintKind::Ip,
        HintKind::Path,
        HintKind::File,
        HintKind::Hash,
        HintKind::Color,
    ];

    /// Config spelling, and the value exported as `$VETER_HINT_KIND`.
    pub fn name(self) -> &'static str {
        match self {
            HintKind::Url => "url",
            HintKind::Email => "email",
            HintKind::Uuid => "uuid",
            HintKind::Ip => "ip",
            HintKind::Path => "path",
            HintKind::File => "file",
            HintKind::Hash => "hash",
            HintKind::Color => "color",
        }
    }

    /// Parse a config name. Case-insensitive; unknown names are the
    /// caller's problem to report.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        HintKind::DEFAULT_ORDER
            .iter()
            .copied()
            .find(|k| k.name() == s)
    }
}

/// One detected span plus what it was recognised as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hint {
    pub span: MatchSpan,
    pub kind: HintKind,
}

/// What [`find_hints`] should look for. Resolved from `[hints]` once at
/// startup; see `config::Config::hint_config`.
#[derive(Clone, Debug, Default)]
pub struct HintConfig {
    /// Enabled detectors, in priority order — earlier wins an overlap.
    pub kinds: Vec<HintKind>,
    /// Extensions accepted by [`HintKind::File`] on top of
    /// [`FILE_EXTENSIONS`], for whatever the built-in list doesn't know
    /// about. Compared case-insensitively, without the leading dot.
    pub extra_file_extensions: Vec<String>,
}

impl HintConfig {
    /// Every detector, nothing extra — the built-in defaults.
    pub fn all() -> Self {
        Self {
            kinds: HintKind::DEFAULT_ORDER.to_vec(),
            extra_file_extensions: Vec::new(),
        }
    }
}

/// Scan `index` for every enabled kind and return the surviving hints in
/// reading order (oldest row first; left to right within a row).
///
/// `cfg.kinds` is both the enable list and the priority order — earlier
/// wins an overlap. Overlap resolution is global per logical line:
/// candidates are considered in (priority, position, length) order and
/// each is kept only if it doesn't touch one already kept. That gives
/// priority absolute precedence, which is what makes
/// `https://host/a1b2c3d` one URL rather than a URL plus a hash.
pub fn find_hints(index: &TextIndex, cfg: &HintConfig) -> Vec<Hint> {
    let kinds = &cfg.kinds;
    let mut out = Vec::new();
    if kinds.is_empty() {
        return out;
    }
    for logical in logical_lines(index) {
        let bytes = logical.text.as_bytes();
        if bytes.is_empty() {
            continue;
        }
        let mut cands: Vec<Cand> = Vec::new();
        for (prio, &kind) in kinds.iter().enumerate() {
            scan(bytes, kind, prio, cfg, &mut cands);
        }
        if cands.is_empty() {
            continue;
        }
        // Priority first, then leftmost, then longest — so a tie between
        // two same-kind candidates (the `://` and `www.` halves of one
        // URL) resolves to the one that starts earlier and covers more.
        cands.sort_by_key(|c| (c.prio, c.start, std::cmp::Reverse(c.end)));
        let mut kept: Vec<Cand> = Vec::new();
        for c in cands {
            if kept.iter().any(|k| c.start < k.end && k.start < c.end) {
                continue;
            }
            kept.push(c);
        }
        kept.sort_by_key(|c| c.start);
        for c in kept {
            let (line, col_start) = logical.locate_start(c.start);
            let (end_line, col_end) = logical.locate_end(c.end);
            // A span that came out empty in cell terms (all-zero-width
            // cells, or a mapping that collapsed) would draw as an
            // invisible highlight with a live label — drop it.
            if end_line == line && col_end <= col_start {
                continue;
            }
            out.push(Hint {
                span: MatchSpan {
                    line,
                    col_start,
                    end_line,
                    col_end,
                },
                kind: c.kind,
            });
        }
    }
    out
}

struct Cand {
    start: usize,
    end: usize,
    kind: HintKind,
    prio: usize,
}

// ---------------------------------------------------------------------------
// Logical lines
// ---------------------------------------------------------------------------

/// One soft-wrap chain, joined for scanning. `parts` maps a byte offset
/// in `text` back to the row it came from, which is how a match range
/// becomes grid coordinates again.
struct Logical<'a> {
    text: String,
    parts: Vec<Part<'a>>,
}

struct Part<'a> {
    /// Offset in [`Logical::text`] where this row's text begins.
    offset: usize,
    row: &'a IndexedRow,
}

impl Logical<'_> {
    /// Row and column a match *starts* at. On a row boundary the later
    /// row wins: byte N is the first byte of the next row, not one past
    /// the end of the previous one.
    fn locate_start(&self, byte: usize) -> (i64, u16) {
        let mut chosen = &self.parts[0];
        for p in &self.parts {
            if p.offset <= byte {
                chosen = p;
            } else {
                break;
            }
        }
        let d = (byte - chosen.offset).min(chosen.row.text.len());
        (chosen.row.line, chosen.row.byte_to_col[d])
    }

    /// Row and exclusive end column a match *ends* at. The mirror of
    /// [`Self::locate_start`]: on a row boundary the earlier row wins, so
    /// a match filling a row ends at that row's past-the-end column
    /// rather than at column 0 of the next.
    fn locate_end(&self, byte: usize) -> (i64, u16) {
        for p in &self.parts {
            if byte <= p.offset + p.row.text.len() {
                return (p.row.line, p.row.byte_to_col[byte - p.offset]);
            }
        }
        let last = self.parts.last().expect("parts is never empty");
        let past_end = last.row.byte_to_col.last().copied().unwrap_or(0);
        (last.row.line, past_end)
    }
}

/// Group `index.rows` into soft-wrap chains. A chain continues while the
/// previous row is flagged `wrapped` *and* the next row is its immediate
/// successor — the index can skip a line that fell out of view, and
/// joining across such a gap would splice unrelated text.
fn logical_lines(index: &TextIndex) -> Vec<Logical<'_>> {
    let mut out: Vec<Logical> = Vec::new();
    let mut prev: Option<&IndexedRow> = None;
    for row in &index.rows {
        let continues = match prev {
            Some(p) => p.wrapped && row.line == p.line + 1,
            None => false,
        };
        if continues {
            let cur = out.last_mut().expect("continues implies a chain exists");
            let offset = cur.text.len();
            cur.text.push_str(&row.text);
            cur.parts.push(Part { offset, row });
        } else {
            out.push(Logical {
                text: row.text.clone(),
                parts: vec![Part { offset: 0, row }],
            });
        }
        prev = Some(row);
    }
    out
}

// ---------------------------------------------------------------------------
// Scanners
// ---------------------------------------------------------------------------
//
// All of these work on bytes. Every pattern they recognise is ASCII, and
// a UTF-8 continuation or lead byte is >= 0x80, which fails every ASCII
// class test — so multi-byte text can never be split mid-character by
// accident. `is_word_byte` deliberately counts >= 0x80 as word-ish, so a
// hex run glued to a non-ASCII word ("café1234567") isn't seen as
// standing alone.

fn scan(t: &[u8], kind: HintKind, prio: usize, cfg: &HintConfig, out: &mut Vec<Cand>) {
    let mut push = |start: usize, end: usize| {
        if end > start {
            out.push(Cand {
                start,
                end,
                kind,
                prio,
            });
        }
    };
    match kind {
        HintKind::Url => scan_urls(t, &mut push),
        HintKind::Email => scan_emails(t, &mut push),
        HintKind::Uuid => scan_uuids(t, &mut push),
        HintKind::Ip => scan_ips(t, &mut push),
        HintKind::Path => scan_paths(t, &mut push),
        HintKind::File => scan_files(t, cfg, &mut push),
        HintKind::Hash => scan_hashes(t, &mut push),
        HintKind::Color => scan_colors(t, &mut push),
    }
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn boundary_before(t: &[u8], i: usize) -> bool {
    i == 0 || !is_word_byte(t[i - 1])
}

fn boundary_after(t: &[u8], i: usize) -> bool {
    i >= t.len() || !is_word_byte(t[i])
}

// --- URL -------------------------------------------------------------

/// Bytes that may appear in a URL body. RFC 3986's reserved + unreserved
/// set, plus `%` for escapes and anything non-ASCII (IDNs and
/// already-decoded paths both show up in terminal output). The tail is
/// tidied afterwards by [`trim_url_tail`].
fn is_url_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~:/?#[]@!$&'()*+,;=%".contains(&b) || b >= 0x80
}

/// Schemes recognised before `://`. A fixed list rather than "any run of
/// scheme-legal bytes", because the generic rule cannot tell a scheme
/// from the tail of the word glued to it — `seehttp://x` walks back into
/// exactly the same shape as `https://x`. Terminals that offer this
/// feature all draw the line here (kitty's `url_prefixes` is the same
/// idea); the cost is that an exotic private scheme goes undetected.
const URL_SCHEMES: &[&[u8]] = &[
    b"http",
    b"https",
    b"ftp",
    b"ftps",
    b"file",
    b"ssh",
    b"sftp",
    b"git",
    b"rsync",
    b"ws",
    b"wss",
    b"irc",
    b"ircs",
    b"gemini",
    b"gopher",
    b"news",
    b"nntp",
    b"redis",
    b"postgres",
    b"postgresql",
    b"mysql",
    b"mongodb",
    b"smb",
    b"nfs",
    b"vnc",
    b"rdp",
    b"s3",
];

fn is_known_scheme(s: &[u8]) -> bool {
    URL_SCHEMES
        .iter()
        .any(|k| k.len() == s.len() && k.eq_ignore_ascii_case(s))
}

fn scan_urls(t: &[u8], push: &mut impl FnMut(usize, usize)) {
    // Scheme-qualified: anchor on "://" and grow both ways.
    let mut i = 0;
    while i < t.len() {
        let Some(rel) = memmem::find(&t[i..], b"://") else {
            break;
        };
        let sep = i + rel;
        let mut start = sep;
        while start > 0
            && matches!(t[start - 1], b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'-' | b'.')
        {
            start -= 1;
        }
        let mut body_end = sep + 3;
        while body_end < t.len() && is_url_byte(t[body_end]) {
            body_end += 1;
        }
        if is_known_scheme(&t[start..sep]) && boundary_before(t, start) {
            let end = trim_url_tail(t, start, body_end);
            if end > sep + 3 {
                push(start, end);
            }
        }
        i = body_end.max(sep + 3);
    }

    // Bare "www." host, the other form people actually paste.
    let mut i = 0;
    while i < t.len() {
        let Some(rel) = memmem::find(&t[i..], b"www.") else {
            break;
        };
        let start = i + rel;
        if boundary_before(t, start) {
            let mut body_end = start + 4;
            while body_end < t.len() && is_url_byte(t[body_end]) {
                body_end += 1;
            }
            let end = trim_url_tail(t, start, body_end);
            // Needs a real label after "www." — "www." alone in prose is
            // not a host.
            if end > start + 4 {
                push(start, end);
            }
            i = body_end.max(start + 4);
        } else {
            i = start + 4;
        }
    }
}

/// Strip what a URL picked up from the prose around it: sentence
/// punctuation, and closing brackets it never opened. The balance check
/// is what keeps a Wikipedia-style `…/Foo_(bar)` intact while still
/// dropping the `)` from `(see https://x/y)`.
fn trim_url_tail(t: &[u8], start: usize, mut end: usize) -> usize {
    while end > start {
        let last = t[end - 1];
        let (open, close) = match last {
            b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' => {
                end -= 1;
                continue;
            }
            b')' => (b'(', b')'),
            b']' => (b'[', b']'),
            b'}' => (b'{', b'}'),
            _ => break,
        };
        let span = &t[start..end];
        let opens = span.iter().filter(|&&b| b == open).count();
        let closes = span.iter().filter(|&&b| b == close).count();
        if closes > opens {
            end -= 1;
        } else {
            break;
        }
    }
    end
}

// --- email -----------------------------------------------------------

fn is_email_local(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"._%+-".contains(&b)
}

fn scan_emails(t: &[u8], push: &mut impl FnMut(usize, usize)) {
    for (i, &b) in t.iter().enumerate() {
        if b != b'@' {
            continue;
        }
        let mut start = i;
        while start > 0 && is_email_local(t[start - 1]) {
            start -= 1;
        }
        // A leading dot or dash isn't part of the address, and the local
        // part has to exist.
        while start < i && matches!(t[start], b'.' | b'-' | b'+') {
            start += 1;
        }
        if start == i || !boundary_before(t, start) {
            continue;
        }
        let Some(end) = domain_end(t, i + 1) else {
            continue;
        };
        push(start, end);
    }
}

/// End of a `label(.label)+` domain starting at `from`, requiring a
/// final all-alphabetic label of at least two chars (the TLD). `None`
/// when there is no such domain, which is what keeps `foo@bar` — a
/// shell variable, not an address — out.
fn domain_end(t: &[u8], from: usize) -> Option<usize> {
    let mut end = from;
    let mut last_dot: Option<usize> = None;
    while end < t.len() && (t[end].is_ascii_alphanumeric() || t[end] == b'-' || t[end] == b'.') {
        if t[end] == b'.' {
            last_dot = Some(end);
        }
        end += 1;
    }
    // Trailing dots/dashes belong to the sentence, not the domain.
    while end > from && matches!(t[end - 1], b'.' | b'-') {
        end -= 1;
        if last_dot == Some(end) {
            let rel = t[from..end].iter().rposition(|&b| b == b'.');
            last_dot = rel.map(|p| p + from);
        }
    }
    let dot = last_dot?;
    let tld = &t[dot + 1..end];
    (tld.len() >= 2 && tld.iter().all(|b| b.is_ascii_alphabetic())).then_some(end)
}

// --- UUID ------------------------------------------------------------

const UUID_GROUPS: [usize; 5] = [8, 4, 4, 4, 12];

fn scan_uuids(t: &[u8], push: &mut impl FnMut(usize, usize)) {
    let total: usize = UUID_GROUPS.iter().sum::<usize>() + 4;
    let mut i = 0;
    while i + total <= t.len() {
        if !boundary_before(t, i) {
            i += 1;
            continue;
        }
        let mut p = i;
        let mut ok = true;
        for (gi, &len) in UUID_GROUPS.iter().enumerate() {
            if gi > 0 {
                if t[p] != b'-' {
                    ok = false;
                    break;
                }
                p += 1;
            }
            if !t[p..p + len].iter().all(|b| b.is_ascii_hexdigit()) {
                ok = false;
                break;
            }
            p += len;
        }
        if ok && boundary_after(t, p) && t.get(p) != Some(&b'-') {
            push(i, p);
            i = p;
        } else {
            i += 1;
        }
    }
}

// --- IPv4 ------------------------------------------------------------

fn scan_ips(t: &[u8], push: &mut impl FnMut(usize, usize)) {
    let mut i = 0;
    while i < t.len() {
        if !t[i].is_ascii_digit() || !boundary_before(t, i) || (i > 0 && t[i - 1] == b'.') {
            i += 1;
            continue;
        }
        let mut p = i;
        let mut octets = 0;
        let mut ok = true;
        while octets < 4 {
            if octets > 0 {
                if t.get(p) != Some(&b'.') {
                    ok = false;
                    break;
                }
                p += 1;
            }
            let digits_at = p;
            while p < t.len() && t[p].is_ascii_digit() && p - digits_at < 3 {
                p += 1;
            }
            let n = p - digits_at;
            if n == 0 || (n > 1 && t[digits_at] == b'0') {
                ok = false;
                break;
            }
            let val: u32 = t[digits_at..p]
                .iter()
                .fold(0u32, |a, &b| a * 10 + u32::from(b - b'0'));
            if val > 255 {
                ok = false;
                break;
            }
            octets += 1;
        }
        if !ok || octets != 4 {
            i += 1;
            continue;
        }
        // An optional `:port` is part of the address people want to act
        // on ("connect to 10.0.0.4:8080"), so keep it.
        let mut end = p;
        if t.get(end) == Some(&b':') {
            let port_at = end + 1;
            let mut q = port_at;
            while q < t.len() && t[q].is_ascii_digit() && q - port_at < 5 {
                q += 1;
            }
            if q > port_at {
                end = q;
            }
        }
        if boundary_after(t, end) && t.get(end) != Some(&b'.') {
            push(i, end);
            i = end;
        } else {
            i = p.max(i + 1);
        }
    }
}

// --- path ------------------------------------------------------------

/// Bytes that end a path token. `:` is *not* here — it is handled
/// specially by [`cut_at_colon`], because it means two different things
/// (`file:42` from grep, and the separator in a `PATH`-style list).
fn is_path_delim(b: u8) -> bool {
    b.is_ascii_whitespace() || b"\"'`<>|(){}[],;=*?".contains(&b)
}

/// Walk `t` as delimited tokens and offer each candidate to `accept`.
///
/// Shared by the path and file scanners, which differ only in which
/// tokens they skip outright and what they consider a name. The per-token
/// work is the fiddly part: trailing sentence punctuation comes off, a
/// `:line[:col]` suffix is preserved, and one token can hold *several*
/// candidates (`PATH=/usr/bin:/usr/local/bin`), so it is walked colon by
/// colon rather than answered once.
fn scan_tokens(
    t: &[u8],
    skip: impl Fn(&[u8]) -> bool,
    accept: impl Fn(&[u8]) -> bool,
    push: &mut impl FnMut(usize, usize),
) {
    let mut i = 0;
    while i < t.len() {
        if is_path_delim(t[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < t.len() && !is_path_delim(t[i]) {
            i += 1;
        }
        let tok_end = i;
        if skip(&t[start..tok_end]) {
            continue;
        }
        let mut cursor = start;
        while cursor < tok_end {
            match token_candidate(t, cursor, tok_end, &accept) {
                Some((s, e)) => {
                    push(s, e);
                    cursor = e.max(cursor + 1);
                }
                None => match t[cursor..tok_end].iter().position(|&b| b == b':') {
                    Some(rel) => cursor += rel + 1,
                    None => break,
                },
            }
            while cursor < tok_end && t[cursor] == b':' {
                cursor += 1;
            }
        }
    }
}

fn scan_paths(t: &[u8], push: &mut impl FnMut(usize, usize)) {
    scan_tokens(
        t,
        // A URL is not a path, and the URL scanner owns the whole token —
        // including the `//host/…` tail, which on its own would read as a
        // perfectly good absolute path and outrank the URL whenever the
        // user puts `path` first in the priority order.
        |tok| memmem::find(tok, b"://").is_some(),
        looks_like_path,
        push,
    );
}

fn scan_files(t: &[u8], cfg: &HintConfig, push: &mut impl FnMut(usize, usize)) {
    scan_tokens(
        t,
        // Anything with a slash is the path scanner's, whether or not it
        // ends in something extension-shaped.
        |tok| tok.contains(&b'/'),
        |tok| looks_like_filename(tok, &cfg.extra_file_extensions),
        push,
    );
}

fn token_candidate(
    t: &[u8],
    start: usize,
    mut end: usize,
    accept: &impl Fn(&[u8]) -> bool,
) -> Option<(usize, usize)> {
    while end > start && matches!(t[end - 1], b'.' | b',' | b';' | b'!' | b'?') {
        end -= 1;
    }
    if end <= start {
        return None;
    }
    let end = cut_at_colon(t, start, end);
    accept(&t[start..end]).then_some((start, end))
}

/// Resolve the `:` in a path token. A trailing `:line[:col]` — grep,
/// rustc, every compiler — stays, because the line number is exactly
/// what makes the hint worth having. Any other colon ends the token, so
/// a `PATH`-style list yields one hint per entry instead of one giant
/// one.
///
/// A location suffix has to be *terminated*: by the end of the token, or
/// by another colon (grep writes `file:12:the matched text`, and rustc
/// writes `file:12:7:`). `file:12abc` is therefore not a location, and
/// the token ends at the colon.
fn cut_at_colon(t: &[u8], start: usize, end: usize) -> usize {
    let Some(rel) = t[start..end].iter().position(|&b| b == b':') else {
        return end;
    };
    let colon = start + rel;
    let digits = |from: usize| {
        let mut q = from;
        while q < end && t[q].is_ascii_digit() {
            q += 1;
        }
        q
    };

    let line_end = digits(colon + 1);
    if line_end == colon + 1 {
        return colon;
    }
    if line_end == end {
        return line_end;
    }
    if t[line_end] != b':' {
        return colon;
    }
    // Another colon follows the line number — the column, if what comes
    // after it is a terminated number too.
    let col_end = digits(line_end + 1);
    if col_end > line_end + 1 && (col_end == end || t[col_end] == b':') {
        col_end
    } else {
        line_end
    }
}

/// Whether a delimited token reads as a path.
///
/// The rule is "contains a slash", with one carve-out: a short
/// single-slash token whose halves are both plain alphanumerics is prose
/// (`and/or`, `TCP/IP`, `24/7`, `km/h`), not a path. That keeps
/// `libs/veter-host` and `src/main.rs` — which have a dash, a dot, or
/// more slashes — while dropping the English cases that would otherwise
/// eat a label letter on every screen of text.
fn looks_like_path(tok: &[u8]) -> bool {
    if tok.starts_with(b"~/") {
        return true;
    }
    let slashes = tok.iter().filter(|&&b| b == b'/').count();
    if slashes == 0 {
        return false;
    }
    // Something has to be there besides the separators.
    if tok.iter().all(|&b| b == b'/' || b == b'.') {
        return false;
    }
    if tok.starts_with(b"/") || tok.starts_with(b"./") || tok.starts_with(b"../") {
        return true;
    }
    if slashes == 1 && tok.len() < 12 {
        let mut halves = tok.splitn(2, |&b| b == b'/');
        let (a, b) = (halves.next().unwrap_or(b""), halves.next().unwrap_or(b""));
        if !a.is_empty()
            && !b.is_empty()
            && a.iter().all(|c| c.is_ascii_alphanumeric())
            && b.iter().all(|c| c.is_ascii_alphanumeric())
        {
            return false;
        }
    }
    true
}

// --- bare filename ---------------------------------------------------

/// Extensions [`HintKind::File`] recognises, without the leading dot and
/// matched case-insensitively.
///
/// An allowlist, not a shape rule, and that is the whole design. A
/// generic `{name}.{ext}` pattern cannot tell `main.rs` from `self.name`
/// or `obj.field`, so reading source or logs in a pager would light up
/// every attribute access on screen and burn the label alphabet on
/// them. Restricting to extensions people actually have files named
/// after costs the occasional exotic suffix — which is what
/// [`HintConfig::extra_file_extensions`] is for — and buys a mode that
/// stays quiet.
///
/// Note that a filename and a bare hostname are the same shape
/// (`example.com` vs `notes.txt`), and `com` is not on this list while
/// `rs` and `md` are also country TLDs. So a bare hostname in output is
/// usually not detected, and one written `example.rs` would be seen as a
/// file. Both are the right trade for a terminal.
// Kept in category rows rather than one-per-line: the grouping is how
// you check whether something is missing.
#[rustfmt::skip]
pub const FILE_EXTENSIONS: &[&str] = &[
    // Source and markup
    "rs", "toml", "lock", "md", "markdown", "txt", "rst", "adoc", "json", "json5", "yaml", "yml",
    "xml", "html", "htm", "css", "scss", "sass", "less", "js", "mjs", "cjs", "jsx", "ts", "tsx",
    "py", "pyi", "rb", "go", "java", "kt", "kts", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "cs",
    "swift", "m", "mm", "php", "pl", "pm", "lua", "vim", "el", "ex", "exs", "erl", "hs", "ml",
    "sql", "r", "jl", "dart", "scala", "clj", "groovy", "gradle", "nim", "zig", "proto",
    "graphql", "tf", "hcl", "asm", "s", "d", "f90", "vue", "svelte", "astro",
    // Shell and config
    "sh", "bash", "zsh", "fish", "ps1", "bat", "cmd", "ini", "cfg", "conf", "config", "env",
    "properties", "desktop", "service", "socket", "rules", "plist", "csv", "tsv",
    // Documents
    "pdf", "doc", "docx", "odt", "ods", "odp", "rtf", "tex", "bib", "epub", "xls", "xlsx", "ppt",
    "pptx", "ipynb", "excalidraw",
    // Images
    "png", "jpg", "jpeg", "gif", "bmp", "svg", "webp", "ico", "tiff", "tif", "avif", "heic",
    "psd", "xcf", "ppm", "pgm", "raw", "dng",
    // Audio and video
    "mp3", "flac", "wav", "ogg", "opus", "m4a", "aac", "wma", "mp4", "mkv", "avi", "mov", "webm",
    "wmv", "flv", "m4v", "mpg", "mpeg", "ts_", "srt", "vtt",
    // Archives and packages
    "zip", "tar", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "deb", "rpm", "pkg", "dmg", "iso",
    "jar", "war", "whl", "apk", "appimage", "snap", "flatpak",
    // Build output and runtime junk
    "o", "a", "so", "dylib", "dll", "exe", "bin", "img", "log", "bak", "tmp", "swp", "pid",
    "patch", "diff", "sum", "mod", "map", "dump", "core", "db", "sqlite", "sqlite3",
    // Fonts
    "ttf", "otf", "woff", "woff2", "eot",
];

fn is_known_extension(ext: &[u8], extra: &[String]) -> bool {
    let known = |e: &str| e.as_bytes().eq_ignore_ascii_case(ext);
    FILE_EXTENSIONS.iter().copied().any(known)
        // A configured extension may be written with or without its dot.
        || extra.iter().any(|e| known(e.trim_start_matches('.')))
}

/// Whether a slash-free token reads as `{name}.{ext}`.
///
/// The extension does the work (see [`FILE_EXTENSIONS`]); the structural
/// rules here only catch what an allowlist can't — dotfiles, which have
/// no extension to check, and one-letter-either-side abbreviations. A
/// `:line[:col]` suffix is ignored for the purposes of finding the
/// extension but stays part of the hint, so `main.rs:42` selects whole.
fn looks_like_filename(tok: &[u8], extra: &[String]) -> bool {
    // Everything from the first colon on is the location suffix —
    // `cut_at_colon` already validated its shape.
    let name = match tok.iter().position(|&b| b == b':') {
        Some(p) => &tok[..p],
        None => tok,
    };
    let Some(dot) = name.iter().rposition(|&b| b == b'.') else {
        return false;
    };
    let (stem, ext) = (&name[..dot], &name[dot + 1..]);
    // A dotfile is all extension (`.bashrc`, `.gitignore`) and no
    // allowlist can hold those, so it gets a shape rule instead: a
    // leading dot followed by one name-shaped, dot-free word.
    if dot == 0 {
        return ext.len() >= 2
            && ext
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            && ext.iter().any(|b| b.is_ascii_alphabetic());
    }
    if !is_known_extension(ext, extra) {
        return false;
    }
    // "e.g", "i.e", "a.m" — one letter either side of a dot is an
    // abbreviation far more often than it is `x.c`.
    if stem.len() == 1 && ext.len() == 1 {
        return false;
    }
    stem.iter().any(|b| b.is_ascii_alphanumeric())
}

// --- hash ------------------------------------------------------------

/// Shortest run accepted as a hash — `git log --abbrev-commit` prints
/// seven.
const HASH_MIN: usize = 7;
/// Longest — a sha256 digest.
const HASH_MAX: usize = 64;

fn scan_hashes(t: &[u8], push: &mut impl FnMut(usize, usize)) {
    let mut i = 0;
    while i < t.len() {
        if !boundary_before(t, i) || !is_lower_hex(t[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < t.len() && is_lower_hex(t[i]) {
            i += 1;
        }
        let len = i - start;
        // Lowercase only (git, docker and the *sum tools all print
        // lowercase), and at least one letter — a run of digits is a
        // timestamp or a byte count far more often than a hash.
        if (HASH_MIN..=HASH_MAX).contains(&len)
            && boundary_after(t, i)
            && t[start..i].iter().any(|b| b.is_ascii_alphabetic())
        {
            push(start, i);
        }
    }
}

fn is_lower_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}

// --- colour ----------------------------------------------------------

fn scan_colors(t: &[u8], push: &mut impl FnMut(usize, usize)) {
    for (i, &b) in t.iter().enumerate() {
        if b != b'#' {
            continue;
        }
        let mut end = i + 1;
        while end < t.len() && t[end].is_ascii_hexdigit() {
            end += 1;
        }
        let len = end - i - 1;
        if matches!(len, 3 | 4 | 6 | 8) && boundary_after(t, end) {
            push(i, end);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::extract_indexed_text;
    use vt100::Parser;

    /// Index one screen's worth of text, then run every detector over it
    /// in the default order.
    fn hints_of(text: &str, cols: u16) -> Vec<(HintKind, String)> {
        hints_with(text, cols, HintKind::DEFAULT_ORDER)
    }

    fn hints_with(text: &str, cols: u16, kinds: &[HintKind]) -> Vec<(HintKind, String)> {
        hints_cfg(
            text,
            cols,
            &HintConfig {
                kinds: kinds.to_vec(),
                extra_file_extensions: Vec::new(),
            },
        )
    }

    fn hints_cfg(text: &str, cols: u16, cfg: &HintConfig) -> Vec<(HintKind, String)> {
        let rows = 24;
        let mut p = Parser::new(rows, cols, 100);
        p.process(text.replace('\n', "\r\n").as_bytes());
        let idx = extract_indexed_text(&mut p, 0);
        find_hints(&idx, cfg)
            .into_iter()
            .map(|h| (h.kind, span_text(&idx, h.span)))
            .collect()
    }

    /// Read a span back out of the index, following a soft wrap — the
    /// same joining `extract_text_from_parser` does for a selection, so
    /// what a test asserts is what a copy would produce.
    fn span_text(idx: &TextIndex, span: MatchSpan) -> String {
        let mut out = String::new();
        for row in &idx.rows {
            if row.line < span.line || row.line > span.end_line {
                continue;
            }
            let from = if row.line == span.line {
                span.col_start
            } else {
                0
            };
            let to = if row.line == span.end_line {
                span.col_end
            } else {
                u16::MAX
            };
            let s = row
                .byte_to_col
                .iter()
                .position(|&c| c >= from)
                .unwrap_or(row.text.len());
            let e = row
                .byte_to_col
                .iter()
                .position(|&c| c >= to)
                .unwrap_or(row.text.len());
            out.push_str(&row.text[s..e.max(s)]);
        }
        out
    }

    fn only(kind: HintKind, hits: &[(HintKind, String)]) -> Vec<String> {
        hits.iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, s)| s.clone())
            .collect()
    }

    #[test]
    fn finds_a_plain_url() {
        let h = hints_of("see https://example.com/a/b for more", 80);
        assert_eq!(only(HintKind::Url, &h), vec!["https://example.com/a/b"]);
    }

    #[test]
    fn url_drops_a_sentence_period() {
        let h = hints_of("read https://example.com/docs.", 80);
        assert_eq!(only(HintKind::Url, &h), vec!["https://example.com/docs"]);
    }

    #[test]
    fn url_keeps_balanced_parens_but_drops_a_wrapping_one() {
        let h = hints_of("(see https://en.wikipedia.org/wiki/Foo_(bar))", 80);
        assert_eq!(
            only(HintKind::Url, &h),
            vec!["https://en.wikipedia.org/wiki/Foo_(bar)"]
        );
    }

    /// The scheme allowlist earns its keep here: a generic "run of
    /// scheme-legal bytes" rule cannot tell `seehttp` from `https`.
    #[test]
    fn url_is_not_glued_to_a_preceding_word() {
        let h = hints_of("seehttp://example.com", 80);
        assert!(only(HintKind::Url, &h).is_empty());
    }

    #[test]
    fn an_unknown_scheme_is_not_a_url() {
        let h = hints_of("myapp://open/thing", 80);
        assert!(only(HintKind::Url, &h).is_empty());
    }

    #[test]
    fn bare_www_host_is_a_url() {
        let h = hints_of("visit www.example.com today", 80);
        assert_eq!(only(HintKind::Url, &h), vec!["www.example.com"]);
    }

    #[test]
    fn www_inside_a_scheme_url_yields_one_hint() {
        let h = hints_of("https://www.example.com/x", 80);
        assert_eq!(only(HintKind::Url, &h), vec!["https://www.example.com/x"]);
    }

    /// The case the whole multi-row machinery exists for: a URL longer
    /// than the terminal is wide comes back as one span, not two halves.
    #[test]
    fn url_spanning_a_soft_wrap_is_one_hint() {
        let url = "https://example.com/very/long/path/that/wraps/around";
        let h = hints_of(url, 20);
        assert_eq!(only(HintKind::Url, &h), vec![url]);
    }

    #[test]
    fn wrapped_span_reports_both_rows() {
        let url = "https://example.com/very/long/path/that/wraps";
        let mut p = Parser::new(24, 20, 100);
        p.process(url.as_bytes());
        let idx = extract_indexed_text(&mut p, 0);
        let only_urls = HintConfig {
            kinds: vec![HintKind::Url],
            ..Default::default()
        };
        let hints = find_hints(&idx, &only_urls);
        assert_eq!(hints.len(), 1);
        let span = hints[0].span;
        assert_eq!(span.line, 0);
        assert_eq!(span.col_start, 0);
        assert!(span.end_line > span.line, "span should cross a row");
    }

    #[test]
    fn finds_a_path_with_a_line_and_column() {
        let h = hints_of("src/main.rs:42:7: error: nope", 80);
        assert_eq!(only(HintKind::Path, &h), vec!["src/main.rs:42:7"]);
    }

    #[test]
    fn finds_a_grep_style_path_with_a_trailing_colon() {
        let h = hints_of("veter/src/search.rs:12:match", 80);
        assert_eq!(only(HintKind::Path, &h), vec!["veter/src/search.rs:12"]);
    }

    #[test]
    fn a_path_list_splits_at_the_colon() {
        let h = hints_of("PATH=/usr/bin:/usr/local/bin", 80);
        let paths = only(HintKind::Path, &h);
        assert_eq!(paths, vec!["/usr/bin", "/usr/local/bin"]);
    }

    #[test]
    fn finds_absolute_and_home_paths() {
        let h = hints_of("cp ~/Downloads/a.txt /tmp/b.txt", 80);
        assert_eq!(
            only(HintKind::Path, &h),
            vec!["~/Downloads/a.txt", "/tmp/b.txt"]
        );
    }

    #[test]
    fn path_keeps_a_directory_without_an_extension() {
        let h = hints_of("cd libs/veter-host", 80);
        assert_eq!(only(HintKind::Path, &h), vec!["libs/veter-host"]);
    }

    #[test]
    fn prose_slashes_are_not_paths() {
        let h = hints_of("and/or TCP/IP 24/7 read/write", 80);
        assert!(only(HintKind::Path, &h).is_empty(), "{h:?}");
    }

    #[test]
    fn path_is_unwrapped_from_quotes_and_parens() {
        let h = hints_of("note (see /etc/hosts) and \"/var/log/x.log\"", 80);
        assert_eq!(
            only(HintKind::Path, &h),
            vec!["/etc/hosts", "/var/log/x.log"]
        );
    }

    #[test]
    fn finds_bare_filenames() {
        let h = hints_of("edit Cargo.toml and README.md now", 80);
        assert_eq!(only(HintKind::File, &h), vec!["Cargo.toml", "README.md"]);
    }

    #[test]
    fn a_bare_filename_keeps_its_line_number() {
        let h = hints_of("main.rs:42:7: error: nope", 80);
        assert_eq!(only(HintKind::File, &h), vec!["main.rs:42:7"]);
    }

    /// A filename inside a path belongs to the path — the two scanners
    /// split on the slash, so neither can double-report.
    #[test]
    fn a_filename_inside_a_path_is_not_reported_twice() {
        let h = hints_of("open veter/src/main.rs now", 80);
        assert_eq!(only(HintKind::Path, &h), vec!["veter/src/main.rs"]);
        assert!(only(HintKind::File, &h).is_empty());
    }

    /// The reason the extension is an allowlist rather than a shape: a
    /// screenful of source would otherwise label every field access.
    #[test]
    fn attribute_access_is_not_a_filename() {
        let h = hints_of("self.name = obj.field; foo.bar", 80);
        assert!(only(HintKind::File, &h).is_empty(), "{h:?}");
    }

    #[test]
    fn version_numbers_and_abbreviations_are_not_filenames() {
        let h = hints_of("veter 0.1.0, e.g. 3.14 and i.e. 1.2.3", 80);
        assert!(only(HintKind::File, &h).is_empty(), "{h:?}");
    }

    #[test]
    fn finds_dotfiles() {
        let h = hints_of("cat .gitignore .bashrc", 80);
        assert_eq!(only(HintKind::File, &h), vec![".gitignore", ".bashrc"]);
    }

    #[test]
    fn an_ellipsis_is_not_a_dotfile() {
        let h = hints_of("wait ... done", 80);
        assert!(only(HintKind::File, &h).is_empty(), "{h:?}");
    }

    #[test]
    fn extra_extensions_extend_the_allowlist() {
        let cfg = HintConfig {
            kinds: vec![HintKind::File],
            extra_file_extensions: vec![".myext".into(), "other".into()],
        };
        let h = hints_cfg("open thing.myext and a.other and b.nope", 80, &cfg);
        assert_eq!(
            only(HintKind::File, &h),
            vec!["thing.myext", "a.other"],
            "{h:?}"
        );
    }

    /// Extensions are matched case-insensitively — `ls` on a camera dump
    /// is full of `.JPG`.
    #[test]
    fn extensions_ignore_case() {
        let h = hints_of("DSC_0001.JPG IMG.Png", 80);
        assert_eq!(only(HintKind::File, &h), vec!["DSC_0001.JPG", "IMG.Png"]);
    }

    #[test]
    fn finds_a_git_hash() {
        let h = hints_of("commit 9e122cb feat: veter", 80);
        assert_eq!(only(HintKind::Hash, &h), vec!["9e122cb"]);
    }

    #[test]
    fn digits_alone_are_not_a_hash() {
        let h = hints_of("total 12345678 bytes", 80);
        assert!(only(HintKind::Hash, &h).is_empty());
    }

    #[test]
    fn a_hash_inside_a_url_loses_to_the_url() {
        let h = hints_of("https://git.example.com/c/9e122cbf", 80);
        assert_eq!(only(HintKind::Hash, &h), Vec::<String>::new());
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, HintKind::Url);
    }

    #[test]
    fn finds_an_email_not_a_path() {
        let h = hints_of("from rtgbnm@gmail.com today", 80);
        assert_eq!(only(HintKind::Email, &h), vec!["rtgbnm@gmail.com"]);
    }

    #[test]
    fn a_shell_variable_is_not_an_email() {
        let h = hints_of("user@host runs it", 80);
        assert!(only(HintKind::Email, &h).is_empty());
    }

    #[test]
    fn finds_an_ip_with_a_port() {
        let h = hints_of("listening on 10.0.0.4:8080 now", 80);
        assert_eq!(only(HintKind::Ip, &h), vec!["10.0.0.4:8080"]);
    }

    #[test]
    fn a_version_number_is_not_an_ip() {
        let h = hints_of("veter 0.1.0 built", 80);
        assert!(only(HintKind::Ip, &h).is_empty());
    }

    #[test]
    fn finds_a_uuid() {
        let h = hints_of("id 60e1bb0b-f091-4c09-94c6-9a6a950cd113 ok", 80);
        assert_eq!(
            only(HintKind::Uuid, &h),
            vec!["60e1bb0b-f091-4c09-94c6-9a6a950cd113"]
        );
    }

    #[test]
    fn finds_a_hex_color() {
        let h = hints_of("accent #56799f and #fff", 80);
        assert_eq!(only(HintKind::Color, &h), vec!["#56799f", "#fff"]);
    }

    #[test]
    fn disabled_kinds_produce_nothing() {
        let h = hints_with(
            "see https://example.com/a and /etc/hosts",
            80,
            &[HintKind::Path],
        );
        assert_eq!(only(HintKind::Path, &h), vec!["/etc/hosts"]);
        assert!(only(HintKind::Url, &h).is_empty());
    }

    /// Priority is the caller's order, not a built-in one: put path
    /// first and the URL's path-shaped tail no longer loses to it.
    #[test]
    fn priority_follows_the_configured_order() {
        let default = hints_of("https://example.com/a/b", 80);
        assert_eq!(default.len(), 1);
        assert_eq!(default[0].0, HintKind::Url);

        let flipped = hints_with(
            "https://example.com/a/b",
            80,
            &[HintKind::Path, HintKind::Url],
        );
        // The path scanner rejects a token holding "://", so the URL
        // still wins — the point here is that nothing is lost or
        // duplicated when the order changes.
        assert_eq!(flipped.len(), 1);
        assert_eq!(flipped[0].0, HintKind::Url);
    }

    #[test]
    fn hints_come_back_in_reading_order() {
        let h = hints_of("/a/one.txt then /b/two.txt\n/c/three.txt", 80);
        assert_eq!(
            only(HintKind::Path, &h),
            vec!["/a/one.txt", "/b/two.txt", "/c/three.txt"]
        );
    }

    #[test]
    fn empty_kind_list_finds_nothing() {
        assert!(hints_with("https://example.com", 80, &[]).is_empty());
    }
}
