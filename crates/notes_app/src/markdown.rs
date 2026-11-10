//! Line classification and inline markdown -> text runs.

use std::ops::Range;

use gpui::{Font, FontStyle, FontWeight, Hsla, StrikethroughStyle, TextRun, px};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

pub const MONO: &str = "Consolas";
/// Obsidian's ==highlight== background (yellow, translucent).
const HIGHLIGHT: gpui::Rgba = gpui::Rgba {
    r: 1.,
    g: 0.82,
    b: 0.,
    a: 0.4,
};

pub enum Kind<'a> {
    Blank,
    Heading(usize, &'a str),
    List {
        indent: usize,
        marker: String,
        task: Option<bool>,
        rest: &'a str,
    },
    Quote(&'a str),
    Rule,
    Image(&'a str),
    Text(&'a str),
}

pub fn classify(line: &str) -> Kind<'_> {
    let t = line.trim_start();
    let tt = t.trim_end();
    if tt.is_empty() {
        return Kind::Blank;
    }
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && matches!(line.as_bytes().get(hashes), None | Some(b' ')) {
        return Kind::Heading(hashes, line[hashes..].trim());
    }
    let first = tt.as_bytes()[0];
    if matches!(first, b'-' | b'*' | b'_')
        && tt.bytes().filter(|&b| b == first).count() >= 3
        && tt.bytes().all(|b| b == first || b == b' ')
    {
        return Kind::Rule;
    }
    if let Some(rest) = t.strip_prefix('>') {
        return Kind::Quote(rest.trim_start());
    }
    if tt.starts_with("![")
        && tt.ends_with(')')
        && let Some(i) = tt.find("](")
    {
        return Kind::Image(&tt[i + 2..tt.len() - 1]);
    }
    let indent = line[..line.len() - t.len()]
        .chars()
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum();
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    let (marker, rest) = if ["- ", "* ", "+ "].iter().any(|b| t.starts_with(b)) {
        ("•".to_string(), &t[2..])
    } else if digits > 0 && (t[digits..].starts_with(". ") || t[digits..].starts_with(") ")) {
        (t[..digits + 1].to_string(), &t[digits + 2..])
    } else {
        return Kind::Text(t);
    };
    let (task, rest) = if let Some(r) = rest.strip_prefix("[ ]") {
        (Some(false), r.trim_start())
    } else if let Some(r) = rest.strip_prefix("[x]").or(rest.strip_prefix("[X]")) {
        (Some(true), r.trim_start())
    } else {
        (None, rest)
    };
    Kind::List {
        indent,
        marker,
        task,
        rest,
    }
}

/// `(length of this line's list prefix, prefix for the next line)` for Enter continuation.
pub fn list_prefix(line: &str) -> Option<(usize, String)> {
    let t = line.trim_start();
    let ind = &line[..line.len() - t.len()];
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    let (own, next) = if digits > 0 && t[digits..].starts_with(". ") {
        (
            digits + 2,
            format!("{}. ", t[..digits].parse::<u64>().ok()? + 1),
        )
    } else if let Some(b) = ["- ", "* ", "+ ", "> "]
        .into_iter()
        .find(|b| t.starts_with(b))
    {
        let r = &t[2..];
        if r.starts_with("[ ] ") || r.starts_with("[x] ") || r.starts_with("[X] ") {
            (6, format!("{b}[ ] "))
        } else {
            (2, b.to_string())
        }
    } else {
        return None;
    };
    Some((ind.len() + own, format!("{ind}{next}")))
}

/// Flips the first `[ ]` / `[x]` in a task line.
pub fn toggle_task(line: &str) -> Option<String> {
    let i = line.find('[')?;
    let new = match line.get(i..i + 3)? {
        "[ ]" => "[x]",
        "[x]" | "[X]" => "[ ]",
        _ => return None,
    };
    Some(format!("{}{new}{}", &line[..i], &line[i + 3..]))
}

pub enum Block {
    Line(usize),
    Fence(Range<usize>),
    Table(Range<usize>),
}

pub fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

/// Language of a fence opener: "```python" → "python".
pub fn fence_info(line: &str) -> &str {
    line.trim_start().trim_start_matches(['`', '~']).trim()
}

fn is_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

fn is_sep(line: &str) -> bool {
    is_row(line)
        && line.contains('-')
        && line
            .trim()
            .chars()
            .all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

pub fn blocks(lines: &[String]) -> Vec<Block> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if is_fence(&lines[i]) {
            let end = (i + 1..lines.len())
                .find(|&j| is_fence(&lines[j]))
                .map_or(lines.len(), |j| j + 1);
            out.push(Block::Fence(i..end));
            i = end;
        } else if is_row(&lines[i]) && lines.get(i + 1).is_some_and(|l| is_sep(l)) {
            let end = (i..lines.len())
                .find(|&j| !is_row(&lines[j]))
                .unwrap_or(lines.len());
            out.push(Block::Table(i..end));
            i = end;
        } else {
            out.push(Block::Line(i));
            i += 1;
        }
    }
    out
}

pub fn cells(line: &str) -> Vec<&str> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(str::trim).collect()
}

pub struct Palette {
    pub accent: Hsla,
    pub dim: Hsla,
    pub code_bg: Hsla,
}

pub struct Inline {
    pub text: String,
    pub runs: Vec<TextRun>,
    pub links: Vec<(Range<usize>, String)>,
}

/// Renders inline markdown: markers are dropped, styles become text runs.
pub fn inline(src: &str, base: &Font, color: Hsla, p: &Palette) -> Inline {
    let mut out = Inline {
        text: String::new(),
        runs: Vec::new(),
        links: Vec::new(),
    };
    let (mut bold, mut italic, mut strike, mut image) = (0, 0, 0, 0);
    let (mut mark, mut comment) = (false, false);
    let mut link: Option<(usize, String)> = None;
    for event in Parser::new_ext(src, Options::ENABLE_STRIKETHROUGH) {
        let (s, code) = match event {
            Event::Start(tag) => {
                match tag {
                    Tag::Strong => bold += 1,
                    Tag::Emphasis => italic += 1,
                    Tag::Strikethrough => strike += 1,
                    Tag::Image { .. } => image += 1,
                    Tag::Link { dest_url, .. } => {
                        link = Some((out.text.len(), dest_url.to_string()))
                    }
                    _ => {}
                }
                continue;
            }
            Event::End(tag) => {
                match tag {
                    TagEnd::Strong => bold -= 1,
                    TagEnd::Emphasis => italic -= 1,
                    TagEnd::Strikethrough => strike -= 1,
                    TagEnd::Image => image -= 1,
                    TagEnd::Link => {
                        if let Some((start, url)) = link.take() {
                            out.links.push((start..out.text.len(), url));
                        }
                    }
                    _ => {}
                }
                continue;
            }
            Event::Code(s) => (s, true),
            Event::Text(s)
            | Event::Html(s)
            | Event::InlineHtml(s)
            | Event::InlineMath(s)
            | Event::DisplayMath(s) => (s, false),
            Event::SoftBreak | Event::HardBreak => (" ".into(), false),
            _ => continue,
        };
        let s = s.trim_end_matches('\n');
        if s.is_empty() {
            continue;
        }
        let mut font = base.clone();
        if bold > 0 {
            font.weight = FontWeight::BOLD;
        }
        if italic > 0 {
            font.style = FontStyle::Italic;
        }
        if code {
            font.family = MONO.into();
        }
        // Obsidian extras in plain text: ==highlight== and hidden %%comments%%.
        let mut rest = s;
        while !rest.is_empty() {
            let next = if code {
                None
            } else {
                ["==", "%%"]
                    .iter()
                    .filter_map(|m| rest.find(m).map(|i| (i, *m)))
                    .min()
            };
            let (seg, marker) = match next {
                Some((i, m)) => (&rest[..i], Some(m)),
                None => (rest, None),
            };
            if !seg.is_empty() && !comment {
                out.runs.push(TextRun {
                    len: seg.len(),
                    font: font.clone(),
                    color: if link.is_some() {
                        p.accent
                    } else if image > 0 {
                        p.dim
                    } else {
                        color
                    },
                    background_color: if code {
                        Some(p.code_bg)
                    } else if mark {
                        Some(HIGHLIGHT.into())
                    } else {
                        None
                    },
                    underline: None,
                    strikethrough: (strike > 0).then_some(StrikethroughStyle {
                        thickness: px(1.),
                        color: None,
                    }),
                });
                out.text.push_str(seg);
            }
            match marker {
                Some("==") if !comment => mark = !mark,
                Some("%%") => comment = !comment,
                _ => {}
            }
            rest = &rest[seg.len() + marker.map_or(0, str::len)..];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_and_inline() {
        assert!(matches!(classify("## Hi"), Kind::Heading(2, "Hi")));
        assert!(matches!(classify("#tag"), Kind::Text("#tag")));
        assert!(matches!(classify("---"), Kind::Rule));
        assert!(matches!(
            classify("  - [x] done"),
            Kind::List {
                indent: 2,
                task: Some(true),
                rest: "done",
                ..
            }
        ));
        assert!(matches!(
            classify("3. three"),
            Kind::List { rest: "three", .. }
        ));
        assert!(matches!(classify("![a](p.png)"), Kind::Image("p.png")));
        assert_eq!(list_prefix("1. a"), Some((3, "2. ".into())));
        assert_eq!(list_prefix("- [x] a"), Some((6, "- [ ] ".into())));
        assert_eq!(toggle_task("- [ ] a").as_deref(), Some("- [x] a"));
        assert_eq!(cells("| a | b |"), vec!["a", "b"]);
        let lines: Vec<String> = ["```", "x", "```", "| a |", "|---|", "| 1 |", "t"]
            .map(String::from)
            .into();
        let b = blocks(&lines);
        assert!(
            matches!(&b[..], [Block::Fence(r), Block::Table(t), Block::Line(6)] if *r == (0..3) && *t == (3..6))
        );

        let p = Palette {
            accent: gpui::red(),
            dim: gpui::red(),
            code_bg: gpui::red(),
        };
        let out = inline("**b** `c` [l](u)", &gpui::font("x"), gpui::white(), &p);
        assert_eq!(out.text, "b c l");
        assert_eq!(out.links, vec![(4..5, "u".to_string())]);
        assert_eq!(
            out.runs.iter().map(|r| r.len).sum::<usize>(),
            out.text.len()
        );
    }
}
