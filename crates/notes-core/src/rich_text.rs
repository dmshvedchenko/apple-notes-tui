use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RichDocument {
    pub blocks: Vec<Block>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Block {
    Paragraph(Vec<Inline>),
    Heading1(Vec<Inline>),
    Heading2(Vec<Inline>),
    Heading3(Vec<Inline>),
    BulletList(Vec<ListItem>),
    NumberedList(Vec<ListItem>),
    Quote(Vec<Inline>),
    CodeBlock(String),
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ListItem {
    pub content: Vec<Inline>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Inline {
    Text(String),
    Bold(Vec<Inline>),
    Italic(Vec<Inline>),
    Underline(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Link { label: Vec<Inline>, href: String },
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RichFeature {
    Heading1,
    Heading2,
    Heading3,
    Bold,
    Italic,
    Underline,
    Hyperlink,
    Quote,
    Code,
    BulletList,
    NumberedList,
    MixedAdjacentListTypes,
}

impl fmt::Display for RichFeature {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Heading1 => "heading 1",
            Self::Heading2 => "heading 2",
            Self::Heading3 => "heading 3",
            Self::Bold => "bold",
            Self::Italic => "italic",
            Self::Underline => "underline",
            Self::Hyperlink => "hyperlink",
            Self::Quote => "quote",
            Self::Code => "code",
            Self::BulletList => "bullet list",
            Self::NumberedList => "numbered list",
            Self::MixedAdjacentListTypes => "adjacent mixed list types",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RichTextCapabilities {
    pub heading1: bool,
    pub heading2: bool,
    pub heading3: bool,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub hyperlink: bool,
    pub quote: bool,
    pub code: bool,
    pub bullet_list: bool,
    pub numbered_list: bool,
    pub mixed_adjacent_list_types: bool,
}

impl RichTextCapabilities {
    pub const fn all_supported() -> Self {
        Self {
            heading1: true,
            heading2: true,
            heading3: true,
            bold: true,
            italic: true,
            underline: true,
            hyperlink: true,
            quote: true,
            code: true,
            bullet_list: true,
            numbered_list: true,
            mixed_adjacent_list_types: true,
        }
    }

    pub const fn supports(self, feature: RichFeature) -> bool {
        match feature {
            RichFeature::Heading1 => self.heading1,
            RichFeature::Heading2 => self.heading2,
            RichFeature::Heading3 => self.heading3,
            RichFeature::Bold => self.bold,
            RichFeature::Italic => self.italic,
            RichFeature::Underline => self.underline,
            RichFeature::Hyperlink => self.hyperlink,
            RichFeature::Quote => self.quote,
            RichFeature::Code => self.code,
            RichFeature::BulletList => self.bullet_list,
            RichFeature::NumberedList => self.numbered_list,
            RichFeature::MixedAdjacentListTypes => self.mixed_adjacent_list_types,
        }
    }

    pub fn unsupported_features(self, document: &RichDocument) -> Vec<RichFeature> {
        document
            .features()
            .into_iter()
            .filter(|feature| !self.supports(*feature))
            .collect()
    }
}

impl RichDocument {
    pub fn features(&self) -> Vec<RichFeature> {
        let mut features = BTreeSet::new();
        for block in &self.blocks {
            collect_block_features(block, &mut features);
        }
        for pair in self.blocks.windows(2) {
            if matches!(
                pair,
                [Block::BulletList(_), Block::NumberedList(_)]
                    | [Block::NumberedList(_), Block::BulletList(_)]
            ) {
                features.insert(RichFeature::MixedAdjacentListTypes);
            }
        }
        features.into_iter().collect()
    }
}

fn collect_block_features(block: &Block, features: &mut BTreeSet<RichFeature>) {
    let inlines = match block {
        Block::Paragraph(inlines) => inlines,
        Block::Heading1(inlines) => {
            features.insert(RichFeature::Heading1);
            inlines
        }
        Block::Heading2(inlines) => {
            features.insert(RichFeature::Heading2);
            inlines
        }
        Block::Heading3(inlines) => {
            features.insert(RichFeature::Heading3);
            inlines
        }
        Block::BulletList(items) => {
            features.insert(RichFeature::BulletList);
            for item in items {
                collect_inline_features(&item.content, features);
            }
            return;
        }
        Block::NumberedList(items) => {
            features.insert(RichFeature::NumberedList);
            for item in items {
                collect_inline_features(&item.content, features);
            }
            return;
        }
        Block::Quote(inlines) => {
            features.insert(RichFeature::Quote);
            inlines
        }
        Block::CodeBlock(_) => {
            features.insert(RichFeature::Code);
            return;
        }
    };
    collect_inline_features(inlines, features);
}

fn collect_inline_features(inlines: &[Inline], features: &mut BTreeSet<RichFeature>) {
    for inline in inlines {
        let children = match inline {
            Inline::Text(_) => continue,
            Inline::Bold(children) => {
                features.insert(RichFeature::Bold);
                children
            }
            Inline::Italic(children) => {
                features.insert(RichFeature::Italic);
                children
            }
            Inline::Underline(children) => {
                features.insert(RichFeature::Underline);
                children
            }
            Inline::Strikethrough(children) => children,
            Inline::Link { label, .. } => {
                features.insert(RichFeature::Hyperlink);
                label
            }
        };
        collect_inline_features(children, features);
    }
}
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RichTextError {
    #[error("unsupported HTML element: {0}")]
    UnsupportedElement(String),
    #[error("malformed HTML: {0}")]
    Malformed(String),
}

/// Strict, deliberately small parser. Unknown tags are errors rather than lossily ignored.
pub fn parse_notes_html(input: &str) -> Result<RichDocument, RichTextError> {
    let mut source = input.trim();
    let mut blocks = Vec::new();
    while !source.is_empty() {
        if source.starts_with("<div>") || source.starts_with("<p>") {
            let (inside, rest) = take_element(source)?;
            blocks.push(
                parse_notes_normalized_block(inside)?
                    .unwrap_or(Block::Paragraph(parse_inline(inside)?)),
            );
            source = rest.trim_start();
        } else if source.starts_with("<h1>") {
            let (inside, rest) = take_element(source)?;
            blocks.push(Block::Heading1(parse_inline(inside)?));
            source = rest.trim_start();
        } else if source.starts_with("<h2>") {
            let (inside, rest) = take_element(source)?;
            blocks.push(Block::Heading2(parse_inline(inside)?));
            source = rest.trim_start();
        } else if source.starts_with("<h3>") {
            let (inside, rest) = take_element(source)?;
            blocks.push(Block::Heading3(parse_inline(inside)?));
            source = rest.trim_start();
        } else if source.starts_with("<blockquote>") {
            let (inside, rest) = take_element(source)?;
            blocks.push(Block::Quote(parse_inline(inside)?));
            source = rest.trim_start();
        } else if source.starts_with("<pre>") {
            let (inside, rest) = take_element(source)?;
            blocks.push(Block::CodeBlock(unescape(inside)));
            source = rest.trim_start();
        } else if source.starts_with("<ul>") || source.starts_with("<ol>") {
            let ordered = source.starts_with("<ol>");
            let (inside, rest) = take_element(source)?;
            let mut items = Vec::new();
            let mut list = inside.trim();
            while !list.is_empty() {
                if !list.starts_with("<li>") {
                    return Err(RichTextError::UnsupportedElement(
                        "nested or non-li list content".into(),
                    ));
                }
                let (item, next) = take_element(list)?;
                items.push(ListItem {
                    content: parse_inline(item)?,
                });
                list = next.trim();
            }
            blocks.push(if ordered {
                Block::NumberedList(items)
            } else {
                Block::BulletList(items)
            });
            source = rest.trim_start();
        } else if source.starts_with("<br>")
            || source.starts_with("<br/>")
            || source.starts_with("<br />")
        {
            blocks.push(Block::Paragraph(vec![]));
            source = source
                .strip_prefix("<br>")
                .or_else(|| source.strip_prefix("<br/>"))
                .or_else(|| source.strip_prefix("<br />"))
                .unwrap_or("");
        } else if source.starts_with('<') {
            return Err(RichTextError::UnsupportedElement(tag_name(source)));
        } else {
            let split = source.find('<').unwrap_or(source.len());
            let (text, rest) = source.split_at(split);
            blocks.push(Block::Paragraph(vec![Inline::Text(unescape(text))]));
            source = rest.trim_start();
        }
    }
    Ok(RichDocument { blocks })
}
fn take_element(source: &str) -> Result<(&str, &str), RichTextError> {
    let end_open = source
        .find('>')
        .ok_or_else(|| RichTextError::Malformed("missing >".into()))?;
    let name = tag_name(&source[..=end_open]);
    let mut depth = 1_usize;
    let mut cursor = end_open + 1;
    while let Some(relative_start) = source[cursor..].find('<') {
        let start = cursor + relative_start;
        let end = source[start..]
            .find('>')
            .map(|offset| start + offset)
            .ok_or_else(|| RichTextError::Malformed("missing >".into()))?;
        let token = source[start + 1..end].trim();
        if tag_name(token.trim_start_matches('/')) == name {
            if token.starts_with('/') {
                depth -= 1;
                if depth == 0 {
                    return Ok((&source[end_open + 1..start], &source[end + 1..]));
                }
            } else if !token.ends_with('/') {
                depth += 1;
            }
        }
        cursor = end + 1;
    }
    Err(RichTextError::Malformed(format!(
        "missing closing tag for <{name}>"
    )))
}
fn parse_inline(source: &str) -> Result<Vec<Inline>, RichTextError> {
    let mut out = Vec::new();
    let mut rest = source;
    while !rest.is_empty() {
        if rest.starts_with("<br>") || rest.starts_with("<br/>") || rest.starts_with("<br />") {
            out.push(Inline::Text("\n".into()));
            rest = rest
                .strip_prefix("<br>")
                .or_else(|| rest.strip_prefix("<br/>"))
                .or_else(|| rest.strip_prefix("<br />"))
                .unwrap_or("");
        } else if rest.starts_with("<b>") || rest.starts_with("<strong>") {
            let (x, r) = take_element(rest)?;
            out.push(Inline::Bold(parse_inline(x)?));
            rest = r;
        } else if rest.starts_with("<i>") || rest.starts_with("<em>") {
            let (x, r) = take_element(rest)?;
            out.push(Inline::Italic(parse_inline(x)?));
            rest = r;
        } else if rest.starts_with("<u>") {
            let (x, r) = take_element(rest)?;
            out.push(Inline::Underline(parse_inline(x)?));
            rest = r;
        } else if rest.starts_with("<s>") || rest.starts_with("<strike>") {
            let (x, r) = take_element(rest)?;
            out.push(Inline::Strikethrough(parse_inline(x)?));
            rest = r;
        } else if rest.starts_with("<a ") {
            let end = rest
                .find('>')
                .ok_or_else(|| RichTextError::Malformed("link".into()))?;
            let opening = &rest[..=end];
            let href = opening
                .split("href=\"")
                .nth(1)
                .and_then(|x| x.split('"').next())
                .ok_or_else(|| RichTextError::Malformed("link href".into()))?;
            let (inside, remainder) = take_element(rest)?;
            out.push(Inline::Link {
                label: parse_inline(inside)?,
                href: unescape(href),
            });
            rest = remainder;
        } else if starts_tag(rest, "span") || starts_tag(rest, "font") || starts_tag(rest, "tt") {
            let (inside, remainder) = take_element(rest)?;
            out.extend(parse_inline(inside)?);
            rest = remainder;
        } else if rest.starts_with('<') {
            return Err(RichTextError::UnsupportedElement(tag_name(rest)));
        } else {
            let split = rest.find('<').unwrap_or(rest.len());
            let (text, next) = rest.split_at(split);
            if !text.is_empty() {
                out.push(Inline::Text(unescape(text)));
            }
            rest = next;
        }
    }
    Ok(out)
}

fn parse_notes_normalized_block(source: &str) -> Result<Option<Block>, RichTextError> {
    let source = source.trim();
    if source.starts_with("<b>") {
        let (bold_inside, bold_rest) = take_element(source)?;
        let span = bold_inside.trim();
        if bold_rest.trim().is_empty()
            && starts_tag(span, "span")
            && opening_tag(span) == r#"<span style="font-size: 18px">"#
        {
            let (heading_inside, heading_rest) = take_element(span)?;
            if heading_rest.trim().is_empty() {
                return Ok(Some(Block::Heading2(parse_inline(heading_inside)?)));
            }
        }
    }
    if starts_tag(source, "b") || starts_tag(source, "strong") {
        let (bold_inside, bold_rest) = take_element(source)?;
        let span = bold_inside.trim();
        if bold_rest.trim().is_empty()
            && starts_tag(span, "span")
            && opening_tag(span)
                .to_ascii_lowercase()
                .contains("font-size: 24px")
        {
            let (heading_inside, heading_rest) = take_element(span)?;
            if heading_rest.trim().is_empty() {
                return Ok(Some(Block::Heading1(parse_inline(heading_inside)?)));
            }
        }
    }
    if starts_tag(source, "font") && opening_tag(source).to_ascii_lowercase().contains("courier") {
        let (font_inside, font_rest) = take_element(source)?;
        let tt = font_inside.trim();
        if font_rest.trim().is_empty() && starts_tag(tt, "tt") {
            let (code_inside, code_rest) = take_element(tt)?;
            if code_rest.trim().is_empty() {
                return Ok(Some(Block::CodeBlock(unescape(
                    &code_inside
                        .replace("<br>", "\n")
                        .replace("<br/>", "\n")
                        .replace("<br />", "\n"),
                ))));
            }
        }
    }
    Ok(None)
}

fn starts_tag(source: &str, name: &str) -> bool {
    source.starts_with(&format!("<{name}>")) || source.starts_with(&format!("<{name} "))
}

fn opening_tag(source: &str) -> &str {
    source
        .find('>')
        .map(|end| &source[..=end])
        .unwrap_or(source)
}
pub fn serialize_notes_html(doc: &RichDocument) -> String {
    doc.blocks
        .iter()
        .map(block_html)
        .collect::<Vec<_>>()
        .join("\n")
}
fn block_html(b: &Block) -> String {
    match b {
        Block::Paragraph(x) => format!("<div>{}</div>", inline_html(x)),
        Block::Heading1(x) => format!("<h1>{}</h1>", inline_html(x)),
        Block::Heading2(x) => format!("<h2>{}</h2>", inline_html(x)),
        Block::Heading3(x) => format!("<h3>{}</h3>", inline_html(x)),
        Block::Quote(x) => format!("<blockquote>{}</blockquote>", inline_html(x)),
        Block::CodeBlock(x) => format!("<pre>{}</pre>", escape(x)),
        Block::BulletList(x) => format!(
            "<ul>{}</ul>",
            x.iter()
                .map(|i| format!("<li>{}</li>", inline_html(&i.content)))
                .collect::<String>()
        ),
        Block::NumberedList(x) => format!(
            "<ol>{}</ol>",
            x.iter()
                .map(|i| format!("<li>{}</li>", inline_html(&i.content)))
                .collect::<String>()
        ),
    }
}
fn inline_html(x: &[Inline]) -> String {
    x.iter()
        .map(|v| match v {
            Inline::Text(s) => escape(s).replace('\n', "<br>"),
            Inline::Bold(x) => format!("<b>{}</b>", inline_html(x)),
            Inline::Italic(x) => format!("<i>{}</i>", inline_html(x)),
            Inline::Underline(x) => format!("<u>{}</u>", inline_html(x)),
            Inline::Strikethrough(x) => format!("<s>{}</s>", inline_html(x)),
            Inline::Link { label, href } => {
                format!("<a href=\"{}\">{}</a>", escape(href), inline_html(label))
            }
        })
        .collect()
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn unescape(s: &str) -> String {
    s.replace("&nbsp;", "\u{00a0}")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}
fn tag_name(s: &str) -> String {
    s.trim_start_matches('<')
        .split([' ', '>'])
        .next()
        .unwrap_or("unknown")
        .trim_start_matches('/')
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(html: &str) {
        let parsed = parse_notes_html(html).expect(html);
        assert_eq!(
            parse_notes_html(&serialize_notes_html(&parsed)).expect(html),
            parsed,
            "{html}"
        );
    }

    #[test]
    fn semantic_round_trip_fixtures() {
        for html in [
            "<div>plain</div>",
            "<div>one</div><div>two</div>",
            "<div></div><br><div></div>",
            "<div>Семья, Grüße 😀</div>",
            "<div><b>bold</b> <i>italic</i><u>u</u><s>s</s></div>",
            "<div><b>bold <i>nested</i></b> tail</div>",
            "<h1>a</h1><h2>b</h2><h3>c</h3>",
            "<div><a href=\"https://example.test/?a=1&amp;b=2\">link</a></div>",
            "<ul><li>one</li><li></li></ul><ol><li>two</li></ol>",
            "<blockquote>quote</blockquote><pre>a &lt; b\n  code</pre>",
            "<div>&lt;&amp;&quot;</div>",
            "<div><strong>b</strong><em>i</em></div>",
        ] {
            round_trip(html);
        }
    }

    #[test]
    fn does_not_merge_sibling_elements() {
        assert_eq!(
            parse_notes_html("<div>one</div><div>two</div>")
                .unwrap()
                .blocks
                .len(),
            2
        );
    }

    #[test]
    fn rejects_unsafe_and_nested_content() {
        for html in [
            "<table></table>",
            "<div><img></div>",
            "<object></object>",
            "<input>",
            "<ul><li>x<ul><li>y</li></ul></li></ul>",
        ] {
            assert!(matches!(
                parse_notes_html(html),
                Err(RichTextError::UnsupportedElement(_))
            ));
        }
    }

    #[test]
    fn malformed_html_is_explicit() {
        assert!(matches!(
            parse_notes_html("<div>x"),
            Err(RichTextError::Malformed(_))
        ));
    }

    #[test]
    fn parses_real_notes_app_phase4_probe_html() {
        let rich_html = concat!(
            "<div>Phase4-Rich-20260827-170059</div>\n",
            "<div><b><span style=\"font-size: 24px\">Phase 4 Rich Heading</span></b></div>\n",
            "<div><b>Bold paragraph via TUI</b><br></div>\n",
            "<div><i>Italic paragraph via TUI</i><br></div>\n",
            "<div><u>Underline paragraph via TUI</u><br></div>\n",
            "<div><u>Example link via TUI</u><br></div>\n",
            "<div>Русский текст: Привет, мир</div>\n",
            "<div>Deutsch: Grüße Größe über</div>\n",
            "<div>Emoji: 🚀✨</div>\n",
            "<div>Quote preserved by Notes</div>\n",
            "<div><font face=\"Courier\"><tt>Code sample: let x = 42;</tt></font></div>\n",
        );
        let document = parse_notes_html(rich_html).expect("Notes.app normalized rich HTML");
        assert!(matches!(
            document.blocks.as_slice(),
            [
                Block::Paragraph(_),
                Block::Heading1(_),
                Block::Paragraph(_),
                Block::Paragraph(_),
                Block::Paragraph(_),
                Block::Paragraph(_),
                Block::Paragraph(_),
                Block::Paragraph(_),
                Block::Paragraph(_),
                Block::Paragraph(_),
                Block::CodeBlock(code),
            ] if code == "Code sample: let x = 42;"
        ));
        assert!(matches!(
            &document.blocks[4],
            Block::Paragraph(items) if matches!(&items[..], [Inline::Underline(_), Inline::Text(line_break)] if line_break == "\n")
        ));
        assert!(matches!(
            &document.blocks[5],
            Block::Paragraph(items) if matches!(&items[..], [Inline::Underline(_), Inline::Text(line_break)] if line_break == "\n")
        ));
        for (index, expected) in [
            (6, "Русский текст: Привет, мир"),
            (7, "Deutsch: Grüße Größe über"),
            (8, "Emoji: 🚀✨"),
            (9, "Quote preserved by Notes"),
        ] {
            assert!(matches!(
                &document.blocks[index],
                Block::Paragraph(items) if matches!(&items[..], [Inline::Text(text)] if text == expected)
            ));
        }

        let list_html = concat!(
            "<div>Phase4-List-20260827-170059</div>\n",
            "<ul>\n",
            "<li>Bullet alpha via TUI</li>\n",
            "<li>Bullet beta via TUI</li>\n",
            "<li>Numbered one via TUI</li>\n",
            "<li>Numbered two via TUI</li>\n",
            "</ul>\n",
        );
        let list = parse_notes_html(list_html).expect("Notes.app normalized list HTML");
        assert!(matches!(
            list.blocks.as_slice(),
            [Block::Paragraph(_), Block::BulletList(items)] if items.len() == 4
        ));
    }

    #[test]
    fn parses_only_the_observed_notes_app_h2_normalization_as_heading2() {
        let document = parse_notes_html(concat!(
            "<div>Phase41-Probe-H2-20260827</div>\n",
            "<div><b><span style=\"font-size: 18px\">Probe H2</span></b></div>",
        ))
        .expect("exact Notes.app H2 read-back");
        assert!(matches!(
            document.blocks.as_slice(),
            [Block::Paragraph(_), Block::Heading2(items)]
                if matches!(&items[..], [Inline::Text(text)] if text == "Probe H2")
        ));

        for html in [
            "<div><b>Probe H3</b></div>",
            "<div><b><span style=\"font-size: 17px\">not H2</span></b></div>",
            "<div><strong><span style=\"font-size: 18px\">not H2</span></strong></div>",
        ] {
            assert!(matches!(
                parse_notes_html(html).unwrap().blocks.as_slice(),
                [Block::Paragraph(_)]
            ));
        }
    }

    #[test]
    fn feature_analysis_is_recursive_and_mixed_lists_require_direct_adjacency() {
        let nested = RichDocument {
            blocks: vec![
                Block::Heading3(vec![Inline::Bold(vec![Inline::Link {
                    label: vec![Inline::Italic(vec![Inline::Underline(vec![Inline::Text(
                        "nested".into(),
                    )])])],
                    href: "https://example.test".into(),
                }])]),
                Block::Quote(vec![Inline::Text("quote".into())]),
                Block::CodeBlock("code".into()),
                Block::BulletList(vec![ListItem {
                    content: vec![Inline::Text("bullet".into())],
                }]),
                Block::NumberedList(vec![ListItem {
                    content: vec![Inline::Text("numbered".into())],
                }]),
            ],
        };
        assert_eq!(
            nested.features(),
            vec![
                RichFeature::Heading3,
                RichFeature::Bold,
                RichFeature::Italic,
                RichFeature::Underline,
                RichFeature::Hyperlink,
                RichFeature::Quote,
                RichFeature::Code,
                RichFeature::BulletList,
                RichFeature::NumberedList,
                RichFeature::MixedAdjacentListTypes,
            ]
        );

        let separated = RichDocument {
            blocks: vec![
                Block::BulletList(vec![]),
                Block::Paragraph(vec![]),
                Block::NumberedList(vec![]),
            ],
        };
        assert!(!separated
            .features()
            .contains(&RichFeature::MixedAdjacentListTypes));
    }

    #[test]
    fn real_notes_updates_change_only_the_selected_visual_targets() {
        let mut rich_before = parse_notes_html(concat!(
            "<div>Phase4-Rich-20260827-170059</div>\n",
            "<div><b><span style=\"font-size: 24px\">Phase 4 Rich Heading</span></b></div>\n",
            "<div><b>Bold paragraph via TUI</b><br></div>\n",
            "<div><i>Italic paragraph via TUI</i><br></div>\n",
            "<div><u>Underline paragraph via TUI</u><br></div>\n",
            "<div><u>Example link via TUI</u><br></div>\n",
            "<div>Русский текст: Привет, мир</div>\n",
            "<div>Deutsch: Grüße Größe über</div>\n",
            "<div>Emoji: 🚀✨</div>\n",
            "<div>Quote preserved by Notes</div>\n",
            "<div><font face=\"Courier\"><tt>Code sample: let x = 42;</tt></font></div>\n",
        ))
        .expect("rich before");
        rich_before.blocks.remove(0);
        let rich_after = parse_notes_html(concat!(
            "<div><b><span style=\"font-size: 24px\">Phase 4 Rich Heading</span></b></div>\n",
            "<div><b>Bold paragraph via TUI</b><br></div>\n",
            "<div><i>Italic paragraph via TUI</i><br></div>\n",
            "<div><u>Underline paragraph via TUI</u><br></div>\n",
            "<div><u>Example link via TUI</u><br></div>\n",
            "<div>UPDATED Русский текст: Привет, мир</div>\n",
            "<div>Deutsch: Grüße Größe über</div>\n",
            "<div>Emoji: 🚀✨</div>\n",
            "<div>Quote preserved by Notes</div>\n",
            "<div><font face=\"Courier\"><tt>Code sample: let x = 42;</tt></font></div>\n",
        ))
        .expect("rich after");
        assert_eq!(rich_before.blocks.len(), rich_after.blocks.len());
        for index in 0..rich_before.blocks.len() {
            if index == 5 {
                assert_ne!(rich_before.blocks[index], rich_after.blocks[index]);
            } else {
                assert_eq!(rich_before.blocks[index], rich_after.blocks[index]);
            }
        }

        let mut list_before = parse_notes_html(concat!(
            "<div>Phase4-List-20260827-170059</div>\n",
            "<ul><li>Bullet alpha via TUI</li><li>Bullet beta via TUI</li>",
            "<li>Numbered one via TUI</li><li>Numbered two via TUI</li></ul>",
        ))
        .expect("list before");
        list_before.blocks.remove(0);
        let list_after = parse_notes_html(concat!(
            "<ul><li>UPDATED Bullet alpha via TUI</li><li>Bullet beta via TUI</li>",
            "<li>Numbered one via TUI</li><li>Numbered two via TUI</li></ul>",
        ))
        .expect("list after");
        let (Block::BulletList(before), Block::BulletList(after)) =
            (&list_before.blocks[0], &list_after.blocks[0])
        else {
            panic!("expected Notes.app-normalized bullet lists");
        };
        assert_ne!(before[0], after[0]);
        assert_eq!(before[1..], after[1..]);
    }
}
