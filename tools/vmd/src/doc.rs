//! Markdown → the block model vmd lays out.
//!
//! `pulldown-cmark` hands out a flat event stream; this rebuilds the
//! tree from it, because layout needs containers (a list item's blocks
//! indent together, a quote's bar spans everything inside it) and the
//! stream only brackets them.
//!
//! Two deliberate flattenings. **Images are always blocks**: a terminal
//! has no way to set a picture inside a run of prose, so an image ends
//! the paragraph it appeared in and starts a new one after itself.
//! **Raw HTML is dropped**, except `<br>`, which becomes a hard break —
//! its text content still arrives as ordinary `Text` events, so a
//! `<span>`-wrapped phrase reads correctly while the tags themselves
//! don't litter the page.

use std::path::{Path, PathBuf};

use pulldown_cmark::{
    BlockQuoteKind, CodeBlockKind, CowStr, Event, HeadingLevel, LinkType, Options, Parser, Tag,
    TagEnd,
};

/// The font bits an inline run carries. `code` is not a font bit —
/// it selects the code colour in [`crate::layout`] — but it rides here
/// because it nests with the others exactly the same way.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Emphasis {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
}

/// One styled piece of a paragraph, heading or table cell.
#[derive(Clone, Debug)]
pub struct Run {
    pub text: String,
    pub emph: Emphasis,
    /// Index into [`Doc::links`] when this run is part of a link.
    pub link: Option<usize>,
}

#[derive(Clone, Debug)]
pub enum Inline {
    Run(Run),
    /// A hard break (two trailing spaces, a backslash, or `<br>`).
    Break,
}

/// A GFM alert (`> [!NOTE]`), which colours the quote bar and gives it
/// a label.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Alert {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl Alert {
    pub fn label(self) -> &'static str {
        match self {
            Alert::Note => "NOTE",
            Alert::Tip => "TIP",
            Alert::Important => "IMPORTANT",
            Alert::Warning => "WARNING",
            Alert::Caution => "CAUTION",
        }
    }

    /// True for the two that mean "something can go wrong" — they take
    /// the warm tone rather than the accent.
    pub fn is_warning(self) -> bool {
        matches!(self, Alert::Warning | Alert::Caution)
    }

    fn from_cmark(kind: BlockQuoteKind) -> Self {
        match kind {
            BlockQuoteKind::Note => Alert::Note,
            BlockQuoteKind::Tip => Alert::Tip,
            BlockQuoteKind::Important => Alert::Important,
            BlockQuoteKind::Warning => Alert::Warning,
            BlockQuoteKind::Caution => Alert::Caution,
        }
    }
}

/// Column alignment of a table, as the pipe row declared it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug)]
pub struct Item {
    /// `Some` for a task-list item, carrying whether it is ticked.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug)]
pub struct Table {
    pub align: Vec<Align>,
    pub head: Vec<Vec<Inline>>,
    pub rows: Vec<Vec<Vec<Inline>>>,
}

#[derive(Clone, Debug)]
pub struct ImageRef {
    /// Resolved local file, when the URL names one we can open. Remote
    /// URLs stay `None` — vmd fetches nothing over the network.
    pub path: Option<PathBuf>,
    pub url: String,
    pub alt: String,
}

#[derive(Clone, Debug)]
pub enum Block {
    Heading {
        level: u8,
        /// GitHub-style slug, so a `#anchor` link can find this heading.
        slug: String,
        inlines: Vec<Inline>,
    },
    Paragraph(Vec<Inline>),
    Code {
        lang: Option<String>,
        lines: Vec<String>,
    },
    Quote {
        alert: Option<Alert>,
        blocks: Vec<Block>,
    },
    List {
        /// `Some(n)` for an ordered list starting at `n`.
        start: Option<u64>,
        items: Vec<Item>,
    },
    Table(Table),
    Rule,
    Image(ImageRef),
    /// A footnote definition, drawn at the point it was written.
    Footnote {
        label: String,
        blocks: Vec<Block>,
    },
}

#[derive(Clone, Debug)]
pub struct Link {
    pub url: String,
    /// The text between the brackets, for the link picker's label.
    pub text: String,
}

pub struct Doc {
    pub blocks: Vec<Block>,
    pub links: Vec<Link>,
    /// Directory the document was read from — what a relative image or
    /// link URL resolves against. Not the process's working directory,
    /// which is wherever vmd happened to be launched from.
    pub base: PathBuf,
    /// The first `#` heading, when there is one: the window title.
    pub title: Option<String>,
}

/// Everything CommonMark plus the GFM set vmd draws: tables, task
/// lists, strikethrough, footnotes, alerts. Smart punctuation is on
/// because the output is proportional-ish rendered text, not a diff —
/// curly quotes are what the author meant. Metadata blocks are enabled
/// so front matter parses as a metadata block and gets dropped, rather
/// than rendering as a stray paragraph or a thematic break.
fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_SMART_PUNCTUATION
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS
        | Options::ENABLE_GFM
}

/// Parse `text` as the contents of a file in `base`.
pub fn parse(text: &str, base: &Path) -> Doc {
    let mut b = Builder::new(base);
    for event in Parser::new_ext(text, options()) {
        b.event(event);
    }
    b.finish()
}

// ─────────────────────────────────────────────────────────────────────
// Builder
// ─────────────────────────────────────────────────────────────────────

/// One open container. `List` is the odd one — it collects *items*
/// rather than blocks, so it holds no block sink and [`Builder::sink`]
/// looks past it.
enum Frame {
    Quote {
        alert: Option<Alert>,
        blocks: Vec<Block>,
    },
    List {
        start: Option<u64>,
        items: Vec<Item>,
    },
    Item {
        task: Option<bool>,
        blocks: Vec<Block>,
    },
    Footnote {
        label: String,
        blocks: Vec<Block>,
    },
}

/// What text events are currently being collected into.
enum Sink {
    /// The inline accumulator — a paragraph, heading or table cell.
    Inlines,
    /// A fenced or indented code block.
    Code,
    /// An image's alt text.
    Alt,
    /// A metadata block (front matter) — parsed so it doesn't render,
    /// and then thrown away.
    Discard,
}

struct Builder {
    doc: Doc,
    frames: Vec<Frame>,
    inlines: Vec<Inline>,
    emph: Emphasis,
    link: Option<usize>,
    /// Nesting depth of `Tag::Emphasis`/`Strong`/`Strikethrough`, so a
    /// nested pair doesn't clear the bit its parent set.
    depth: EmphDepth,
    sink: Sink,
    code: Option<(Option<String>, String)>,
    image: Option<ImageRef>,
    table: Option<TableBuild>,
    /// Byte offset in the current link's accumulated text, so the link
    /// picker can label an entry with what the reader saw.
    link_text: String,
    /// Heading being collected: its level, and the `{#id}` attribute
    /// when it carried one. The text itself is read off the finished
    /// inlines, so there is nothing to accumulate here.
    heading: Option<(u8, Option<String>)>,
    /// Slugs already used, so duplicates get the `-1`, `-2` suffix
    /// GitHub gives them.
    slugs: Vec<String>,
}

#[derive(Default)]
struct EmphDepth {
    bold: u32,
    italic: u32,
    strike: u32,
}

struct TableBuild {
    align: Vec<Align>,
    head: Vec<Vec<Inline>>,
    rows: Vec<Vec<Vec<Inline>>>,
    row: Vec<Vec<Inline>>,
    in_head: bool,
}

impl Builder {
    fn new(base: &Path) -> Self {
        Builder {
            doc: Doc {
                blocks: Vec::new(),
                links: Vec::new(),
                base: base.to_path_buf(),
                title: None,
            },
            frames: Vec::new(),
            inlines: Vec::new(),
            emph: Emphasis::default(),
            link: None,
            depth: EmphDepth::default(),
            sink: Sink::Inlines,
            code: None,
            image: None,
            table: None,
            link_text: String::new(),
            heading: None,
            slugs: Vec::new(),
        }
    }

    /// Where a finished block goes: the innermost frame that holds
    /// blocks, else the document.
    fn sink_blocks(&mut self) -> &mut Vec<Block> {
        for frame in self.frames.iter_mut().rev() {
            match frame {
                Frame::Quote { blocks, .. }
                | Frame::Item { blocks, .. }
                | Frame::Footnote { blocks, .. } => return blocks,
                Frame::List { .. } => {}
            }
        }
        &mut self.doc.blocks
    }

    fn push_block(&mut self, block: Block) {
        self.sink_blocks().push(block);
    }

    /// Turn whatever inlines are pending into a paragraph.
    ///
    /// A *tight* list item — `- alpha` with no blank line around it —
    /// arrives as bare `Text` events with no `Paragraph` around them
    /// (tightness is a rendering decision the HTML writer makes, so the
    /// event stream simply omits the tag). Without this the text of
    /// every compact list in the document would be dropped on the
    /// floor at `End(Item)`.
    fn flush_paragraph(&mut self) {
        let inlines = self.take_inlines();
        if !inlines.is_empty() {
            self.push_block(Block::Paragraph(inlines));
        }
    }

    /// Append text to whatever is collecting it.
    fn push_text(&mut self, text: &str) {
        match self.sink {
            Sink::Discard => {}
            Sink::Code => {
                if let Some((_, body)) = &mut self.code {
                    body.push_str(text);
                }
            }
            Sink::Alt => {
                if let Some(img) = &mut self.image {
                    img.alt.push_str(text);
                }
            }
            Sink::Inlines => {
                if self.link.is_some() {
                    self.link_text.push_str(text);
                }
                self.push_run(text.to_string(), self.emph);
            }
        }
    }

    /// Append a run, merging into the previous one when nothing about
    /// its styling changed — `**a**b` arrives as two events but is one
    /// run as far as layout cares.
    fn push_run(&mut self, text: String, emph: Emphasis) {
        if text.is_empty() {
            return;
        }
        if let Some(Inline::Run(last)) = self.inlines.last_mut()
            && last.emph == emph
            && last.link == self.link
        {
            last.text.push_str(&text);
            return;
        }
        self.inlines.push(Inline::Run(Run {
            text,
            emph,
            link: self.link,
        }));
    }

    /// Take the inline accumulator, dropping it if it holds nothing but
    /// whitespace.
    fn take_inlines(&mut self) -> Vec<Inline> {
        let inlines = std::mem::take(&mut self.inlines);
        if inlines.iter().all(|i| match i {
            Inline::Run(r) => r.text.trim().is_empty(),
            Inline::Break => true,
        }) {
            return Vec::new();
        }
        inlines
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.push_text(&t),
            Event::Code(t) => {
                let emph = Emphasis {
                    code: true,
                    ..self.emph
                };
                if self.link.is_some() {
                    self.link_text.push_str(&t);
                }
                self.push_run(t.to_string(), emph);
            }
            // Math is parsed only when its option is on, which it
            // isn't; these arms exist so the source stays visible if it
            // ever is.
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                let emph = Emphasis {
                    code: true,
                    ..self.emph
                };
                self.push_run(t.to_string(), emph);
            }
            Event::Html(t) | Event::InlineHtml(t) => self.html(&t),
            Event::FootnoteReference(label) => {
                // No superscript in the protocol, so a reference reads
                // as the marker it is written as, in the code colour.
                let emph = Emphasis {
                    code: true,
                    ..self.emph
                };
                self.push_run(format!("[{label}]"), emph);
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.inlines.push(Inline::Break),
            Event::Rule => self.push_block(Block::Rule),
            Event::TaskListMarker(done) => {
                // Arrives as the first event inside the item's
                // paragraph, after the `Item` frame is already open.
                for frame in self.frames.iter_mut().rev() {
                    if let Frame::Item { task, .. } = frame {
                        *task = Some(done);
                        break;
                    }
                }
            }
        }
    }

    /// Raw HTML. `<br>` is a hard break; everything else is dropped —
    /// see the module comment.
    fn html(&mut self, raw: &str) {
        let t = raw.trim();
        let lower = t.to_ascii_lowercase();
        if lower.starts_with("<br") {
            self.inlines.push(Inline::Break);
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, id, .. } => {
                self.heading = Some((heading_level(level), id.map(|s| s.to_string())));
            }
            Tag::BlockQuote(kind) => self.frames.push(Frame::Quote {
                alert: kind.map(Alert::from_cmark),
                blocks: Vec::new(),
            }),
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => {
                        // A fence can carry more than a language
                        // (`rust,ignore`, `js {highlight}`); the first
                        // word is the part worth showing.
                        let first = l.split([',', ' ', '\t']).next().unwrap_or("").trim();
                        (!first.is_empty()).then(|| first.to_string())
                    }
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((lang, String::new()));
                self.sink = Sink::Code;
            }
            Tag::HtmlBlock => self.sink = Sink::Discard,
            Tag::List(start) => self.frames.push(Frame::List {
                start,
                items: Vec::new(),
            }),
            Tag::Item => self.frames.push(Frame::Item {
                task: None,
                blocks: Vec::new(),
            }),
            Tag::FootnoteDefinition(label) => self.frames.push(Frame::Footnote {
                label: label.to_string(),
                blocks: Vec::new(),
            }),
            Tag::Table(align) => {
                self.table = Some(TableBuild {
                    align: align.into_iter().map(convert_align).collect(),
                    head: Vec::new(),
                    rows: Vec::new(),
                    row: Vec::new(),
                    in_head: false,
                })
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                    t.row = Vec::new();
                }
            }
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.row = Vec::new();
                }
            }
            Tag::TableCell => self.inlines.clear(),
            Tag::Emphasis => {
                self.depth.italic += 1;
                self.emph.italic = true;
            }
            Tag::Strong => {
                self.depth.bold += 1;
                self.emph.bold = true;
            }
            Tag::Strikethrough => {
                self.depth.strike += 1;
                self.emph.strike = true;
            }
            // No baseline shift in the protocol; sub/superscript read
            // as their text.
            Tag::Superscript | Tag::Subscript => {}
            Tag::Link {
                link_type,
                dest_url,
                id,
                ..
            } => {
                let url = resolve_dest(link_type, &dest_url, &id);
                self.doc.links.push(Link {
                    url,
                    text: String::new(),
                });
                self.link = Some(self.doc.links.len() - 1);
                self.link_text.clear();
            }
            Tag::Image {
                dest_url, title, ..
            } => {
                self.image = Some(ImageRef {
                    path: resolve_local(&self.doc.base, &dest_url),
                    url: dest_url.to_string(),
                    alt: title.to_string(),
                });
                // The `title` seeded above is the fallback; alt text
                // events overwrite it if there are any.
                if let Some(img) = &mut self.image {
                    img.alt.clear();
                }
                self.sink = Sink::Alt;
            }
            Tag::MetadataBlock(_) => self.sink = Sink::Discard,
            Tag::DefinitionList => {}
            Tag::DefinitionListTitle => {}
            Tag::DefinitionListDefinition => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                let inlines = self.take_inlines();
                if !inlines.is_empty() {
                    self.push_block(Block::Paragraph(inlines));
                }
            }
            TagEnd::Heading(_) => {
                let (level, id) = self.heading.take().unwrap_or((1, None));
                let inlines = self.take_inlines();
                let plain = plain_text(&inlines);
                if level == 1 && self.doc.title.is_none() && !plain.is_empty() {
                    self.doc.title = Some(plain.clone());
                }
                // An explicit `{#id}` wins over the slug built from the
                // text — that is what an anchor link will name.
                let slug = self.unique_slug(id.unwrap_or_else(|| slugify(&plain)));
                self.push_block(Block::Heading {
                    level,
                    slug,
                    inlines,
                });
            }
            TagEnd::BlockQuote(_) => {
                self.flush_paragraph();
                if let Some(Frame::Quote { alert, blocks }) = self.frames.pop()
                    && !blocks.is_empty()
                {
                    self.push_block(Block::Quote { alert, blocks });
                }
            }
            TagEnd::CodeBlock => {
                self.sink = Sink::Inlines;
                if let Some((lang, body)) = self.code.take() {
                    let mut lines: Vec<String> = body.split('\n').map(expand_tabs).collect();
                    // A fenced block's body ends with the newline before
                    // the closing fence, which is not a blank last line.
                    if lines.last().is_some_and(|l| l.is_empty()) {
                        lines.pop();
                    }
                    if lines.is_empty() {
                        lines.push(String::new());
                    }
                    self.push_block(Block::Code { lang, lines });
                }
            }
            TagEnd::HtmlBlock => self.sink = Sink::Inlines,
            TagEnd::List(_) => {
                if let Some(Frame::List { start, items }) = self.frames.pop()
                    && !items.is_empty()
                {
                    self.push_block(Block::List { start, items });
                }
            }
            TagEnd::Item => {
                self.flush_paragraph();
                if let Some(Frame::Item { task, blocks }) = self.frames.pop()
                    && let Some(Frame::List { items, .. }) = self.frames.last_mut()
                {
                    items.push(Item { task, blocks });
                }
            }
            TagEnd::FootnoteDefinition => {
                self.flush_paragraph();
                if let Some(Frame::Footnote { label, blocks }) = self.frames.pop() {
                    self.push_block(Block::Footnote { label, blocks });
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.push_block(Block::Table(Table {
                        align: t.align,
                        head: t.head,
                        rows: t.rows,
                    }));
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head = std::mem::take(&mut t.row);
                    t.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.row);
                    if t.in_head {
                        t.head = row;
                    } else {
                        t.rows.push(row);
                    }
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.inlines);
                if let Some(t) = &mut self.table {
                    t.row.push(cell);
                }
            }
            TagEnd::Emphasis => {
                self.depth.italic = self.depth.italic.saturating_sub(1);
                self.emph.italic = self.depth.italic > 0;
            }
            TagEnd::Strong => {
                self.depth.bold = self.depth.bold.saturating_sub(1);
                self.emph.bold = self.depth.bold > 0;
            }
            TagEnd::Strikethrough => {
                self.depth.strike = self.depth.strike.saturating_sub(1);
                self.emph.strike = self.depth.strike > 0;
            }
            TagEnd::Superscript | TagEnd::Subscript => {}
            TagEnd::Link => {
                if let Some(i) = self.link.take() {
                    let text = std::mem::take(&mut self.link_text);
                    if let Some(link) = self.doc.links.get_mut(i) {
                        link.text = text;
                    }
                }
            }
            TagEnd::Image => {
                self.sink = Sink::Inlines;
                if let Some(mut img) = self.image.take() {
                    if img.alt.trim().is_empty() {
                        img.alt = file_label(&img.url);
                    }
                    // An image interrupts its paragraph — see the module
                    // comment.
                    let before = self.take_inlines();
                    if !before.is_empty() {
                        self.push_block(Block::Paragraph(before));
                    }
                    self.push_block(Block::Image(img));
                }
            }
            TagEnd::MetadataBlock(_) => self.sink = Sink::Inlines,
            TagEnd::DefinitionList => {}
            TagEnd::DefinitionListTitle | TagEnd::DefinitionListDefinition => {
                let inlines = self.take_inlines();
                if !inlines.is_empty() {
                    self.push_block(Block::Paragraph(inlines));
                }
            }
        }
    }

    /// GitHub's duplicate rule: the second `## Usage` becomes
    /// `usage-1`, so two same-named sections stay separately linkable.
    fn unique_slug(&mut self, base: String) -> String {
        let base = if base.is_empty() {
            "section".to_string()
        } else {
            base
        };
        let mut candidate = base.clone();
        let mut n = 1;
        while self.slugs.contains(&candidate) {
            candidate = format!("{base}-{n}");
            n += 1;
        }
        self.slugs.push(candidate.clone());
        candidate
    }

    fn finish(mut self) -> Doc {
        self.flush_paragraph();
        // A truncated document can leave frames open; close them so
        // their content still renders rather than vanishing.
        while let Some(frame) = self.frames.pop() {
            match frame {
                Frame::Quote { alert, blocks } => {
                    self.sink_blocks().push(Block::Quote { alert, blocks })
                }
                Frame::List { start, items } => {
                    self.sink_blocks().push(Block::List { start, items })
                }
                Frame::Item { task, blocks } => {
                    if let Some(Frame::List { items, .. }) = self.frames.last_mut() {
                        items.push(Item { task, blocks })
                    }
                }
                Frame::Footnote { label, blocks } => {
                    self.sink_blocks().push(Block::Footnote { label, blocks })
                }
            }
        }
        self.doc
    }
}

// ─────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn convert_align(a: pulldown_cmark::Alignment) -> Align {
    match a {
        pulldown_cmark::Alignment::Right => Align::Right,
        pulldown_cmark::Alignment::Center => Align::Center,
        _ => Align::Left,
    }
}

/// A reference link whose definition is missing has an empty
/// `dest_url`; falling back to the reference id at least gives the
/// picker something to show, and an anchor jump something to try.
fn resolve_dest(link_type: LinkType, dest: &CowStr<'_>, id: &CowStr<'_>) -> String {
    if dest.is_empty() {
        match link_type {
            LinkType::ReferenceUnknown | LinkType::CollapsedUnknown | LinkType::ShortcutUnknown => {
                id.to_string()
            }
            _ => String::new(),
        }
    } else {
        dest.to_string()
    }
}

/// The local file a URL names, or `None` for anything vmd will not
/// open off the filesystem (a scheme it doesn't handle, a fragment, a
/// path that isn't there).
pub fn resolve_local(base: &Path, url: &str) -> Option<PathBuf> {
    if url.is_empty() || url.starts_with('#') {
        return None;
    }
    let raw = if let Some(rest) = url.strip_prefix("file://") {
        rest
    } else if has_scheme(url) {
        return None;
    } else {
        url
    };
    // Strip a fragment or query — a path on disk has neither.
    let raw = raw.split(['#', '?']).next().unwrap_or(raw);
    let decoded = percent_decode(raw);
    let path = Path::new(&decoded);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    joined.exists().then_some(joined)
}

/// True when `url` starts with what looks like a URL scheme
/// (`https:`, `mailto:`), as opposed to a Windows-ish drive letter or a
/// bare path.
pub fn has_scheme(url: &str) -> bool {
    match url.find(':') {
        // One letter before the colon is a drive letter far more often
        // than a scheme.
        Some(i) if i > 1 => url[..i]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')),
        _ => false,
    }
}

/// `%20` and friends. Only the escapes a filename realistically
/// carries; anything malformed is left as written.
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// Last path segment of a URL, for an image with no alt text.
fn file_label(url: &str) -> String {
    url.rsplit('/').next().unwrap_or(url).to_string()
}

/// GitHub's heading slug: lowercase, punctuation dropped, spaces to
/// hyphens.
pub fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if c == ' ' || c == '-' || c == '_' {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Flatten inlines to their text, for slugs, titles and the outline.
pub fn plain_text(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for i in inlines {
        match i {
            Inline::Run(r) => out.push_str(&r.text),
            Inline::Break => out.push(' '),
        }
    }
    out.trim().to_string()
}

/// Tabs in a code block are cell-grid alignment, and VGE text has no
/// tab stops — expand them at the conventional 4 so the block keeps the
/// shape its author gave it.
fn expand_tabs(line: &str) -> String {
    const TAB: usize = 4;
    if !line.contains('\t') {
        return line.trim_end_matches('\r').to_string();
    }
    let mut out = String::with_capacity(line.len() + TAB);
    let mut col = 0usize;
    for c in line.trim_end_matches('\r').chars() {
        if c == '\t' {
            let n = TAB - (col % TAB);
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(c);
            col += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Doc {
        parse(text, Path::new("."))
    }

    #[test]
    fn headings_carry_level_slug_and_title() {
        let d = doc("# The Title\n\n## Usage\n\n## Usage\n");
        assert_eq!(d.title.as_deref(), Some("The Title"));
        let slugs: Vec<&str> = d
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Heading { slug, .. } => Some(slug.as_str()),
                _ => None,
            })
            .collect();
        // Duplicate headings get GitHub's numeric suffix, so both stay
        // linkable.
        assert_eq!(slugs, ["the-title", "usage", "usage-1"]);
    }

    #[test]
    fn nested_emphasis_survives_the_inner_close() {
        let d = doc("**bold *both* still bold**\n");
        let Block::Paragraph(inlines) = &d.blocks[0] else {
            panic!("{:?}", d.blocks[0]);
        };
        let runs: Vec<&Run> = inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Run(r) => Some(r),
                _ => None,
            })
            .collect();
        assert!(runs.iter().all(|r| r.emph.bold), "{runs:?}");
        assert_eq!(runs.iter().filter(|r| r.emph.italic).count(), 1);
    }

    #[test]
    fn adjacent_same_style_events_merge_into_one_run() {
        // Smart punctuation splits text at the quote; the pieces still
        // belong to one run, since layout charges a DrawText each.
        let d = doc("plain \"quoted\" plain\n");
        let Block::Paragraph(inlines) = &d.blocks[0] else {
            panic!()
        };
        assert_eq!(inlines.len(), 1, "{inlines:?}");
    }

    /// A tight list carries no `Paragraph` tags at all, so its text
    /// only survives if `End(Item)` flushes the inline accumulator.
    #[test]
    fn a_tight_list_keeps_its_text() {
        let d = doc("- alpha\n- beta\n");
        let Block::List { items, .. } = &d.blocks[0] else {
            panic!("{:?}", d.blocks[0]);
        };
        assert_eq!(items.len(), 2);
        for (item, want) in items.iter().zip(["alpha", "beta"]) {
            let Some(Block::Paragraph(p)) = item.blocks.first() else {
                panic!("{:?}", item.blocks);
            };
            assert_eq!(plain_text(p), want);
        }
    }

    #[test]
    fn lists_nest_and_carry_task_state() {
        let d = doc("- [ ] todo\n- [x] done\n  - nested\n");
        let Block::List { start, items } = &d.blocks[0] else {
            panic!("{:?}", d.blocks[0]);
        };
        assert_eq!(*start, None);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].task, Some(false));
        assert_eq!(items[1].task, Some(true));
        assert!(
            items[1]
                .blocks
                .iter()
                .any(|b| matches!(b, Block::List { .. })),
            "nested list should live inside its parent item"
        );
    }

    #[test]
    fn code_blocks_keep_their_lines_and_first_fence_word() {
        let d = doc("```rust,ignore\nfn main() {}\n\nlet x = 1;\n```\n");
        let Block::Code { lang, lines } = &d.blocks[0] else {
            panic!("{:?}", d.blocks[0]);
        };
        assert_eq!(lang.as_deref(), Some("rust"));
        assert_eq!(lines, &["fn main() {}", "", "let x = 1;"]);
    }

    #[test]
    fn an_image_breaks_the_paragraph_around_it() {
        let d = doc("before ![alt](pic.png) after\n");
        assert!(
            matches!(d.blocks[0], Block::Paragraph(_))
                && matches!(d.blocks[1], Block::Image(_))
                && matches!(d.blocks[2], Block::Paragraph(_)),
            "{:?}",
            d.blocks
        );
        let Block::Image(img) = &d.blocks[1] else {
            panic!()
        };
        assert_eq!(img.alt, "alt");
    }

    #[test]
    fn tables_split_head_from_body() {
        let d = doc("| a | b |\n|---|--:|\n| 1 | 2 |\n| 3 | 4 |\n");
        let Block::Table(t) = &d.blocks[0] else {
            panic!("{:?}", d.blocks[0]);
        };
        assert_eq!(t.head.len(), 2);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.align, [Align::Left, Align::Right]);
    }

    #[test]
    fn front_matter_does_not_render() {
        let d = doc("---\ntitle: hi\n---\n\nbody\n");
        assert_eq!(d.blocks.len(), 1, "{:?}", d.blocks);
        assert!(matches!(d.blocks[0], Block::Paragraph(_)));
    }

    #[test]
    fn html_is_dropped_but_br_breaks_the_line() {
        let d = doc("<div>\nraw\n</div>\n\none<br>two\n");
        let paragraphs: Vec<&Vec<Inline>> = d
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(paragraphs.len(), 1, "{:?}", d.blocks);
        assert!(paragraphs[0].iter().any(|i| matches!(i, Inline::Break)));
    }

    #[test]
    fn links_record_their_text_for_the_picker() {
        let d = doc("see [the spec](doc/spec.md) now\n");
        assert_eq!(d.links.len(), 1);
        assert_eq!(d.links[0].url, "doc/spec.md");
        assert_eq!(d.links[0].text, "the spec");
    }

    #[test]
    fn alerts_become_quote_kinds() {
        let d = doc("> [!WARNING]\n> mind the gap\n");
        let Block::Quote { alert, .. } = &d.blocks[0] else {
            panic!("{:?}", d.blocks[0]);
        };
        assert_eq!(*alert, Some(Alert::Warning));
        assert!(alert.unwrap().is_warning());
    }

    #[test]
    fn scheme_detection_separates_urls_from_paths() {
        assert!(has_scheme("https://example.com"));
        assert!(has_scheme("mailto:a@b.c"));
        assert!(!has_scheme("doc/spec.md"));
        assert!(!has_scheme("./a:b"));
        assert!(!has_scheme("#anchor"));
    }

    #[test]
    fn percent_escapes_decode_in_paths() {
        assert_eq!(percent_decode("a%20b.png"), "a b.png");
        assert_eq!(percent_decode("plain.png"), "plain.png");
        // Malformed escapes are left exactly as written rather than
        // eaten.
        assert_eq!(percent_decode("100%zz"), "100%zz");
    }

    #[test]
    fn tabs_expand_to_four_column_stops() {
        assert_eq!(expand_tabs("\tx"), "    x");
        assert_eq!(expand_tabs("ab\tx"), "ab  x");
        assert_eq!(expand_tabs("abcd\tx"), "abcd    x");
    }

    #[test]
    fn an_unclosed_container_still_yields_its_content() {
        let d = doc("> quote with no blank line after");
        assert!(matches!(d.blocks[0], Block::Quote { .. }), "{:?}", d.blocks);
    }
}
