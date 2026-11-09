//! Obsidian-style live preview for Markdown inside the normal editor: markdown syntax is folded
//! away on every line that doesn't hold a cursor/selection, and shown raw on the ones that do.

use std::{any::TypeId, ops::Range, path::PathBuf, sync::Arc, time::Duration};

use collections::HashSet;
use editor::{
    Addon, Anchor, EphemeralFold, Editor, EditorEvent, FoldPlaceholder, HighlightKey,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, Crease, CustomBlockId, FoldId},
};
use gpui::{
    App, Context, FontWeight, HighlightStyle, ImageSource, MouseButton, Task, Window,
    img,
};
use language::Point;
use pulldown_cmark::{Event, Options, Parser, Tag};
use ui::prelude::*;

const IMAGE_LINES: u32 = 12;

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, window, cx| {
        let Some(window) = window else { return };
        if !editor.mode().is_full() || editor.buffer().read(cx).as_singleton().is_none() {
            return;
        }
        let this = cx.entity();
        let subscription = cx.subscribe_in(&this, window, |editor, _, event: &EditorEvent, window, cx| {
            if matches!(
                event,
                EditorEvent::SelectionsChanged { .. }
                    | EditorEvent::BufferEdited
                    | EditorEvent::Reparsed(_)
            ) {
                schedule(editor, window, cx);
            }
        });
        editor.register_addon(LivePreview {
            _subscription: subscription,
            task: None,
            key: None,
            images: Vec::new(),
            blocks: HashSet::default(),
        });
        schedule(editor, window, cx);
    })
    .detach();
}

struct LivePreview {
    _subscription: gpui::Subscription,
    task: Option<Task<()>>,
    /// (buffer version, cursor rows) of the last applied state; skips redundant work.
    key: Option<(clock::Global, Vec<u32>)>,
    images: Vec<(u32, String)>,
    blocks: HashSet<CustomBlockId>,
}

impl Addon for LivePreview {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
    fn to_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

fn schedule(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let task = cx.spawn_in(window, async move |this, cx| {
        cx.background_executor()
            .timer(Duration::from_millis(30))
            .await;
        this.update_in(cx, |editor, window, cx| refresh(editor, window, cx))
            .ok();
    });
    if let Some(state) = editor.addon_mut::<LivePreview>() {
        state.task = Some(task);
    }
}

// ponytail: re-folds the whole buffer on every cursor-row change; fine for notes, diff per-row if huge files lag.
fn refresh(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let buffer = buffer.read(cx);
    let is_markdown = buffer.language().is_some_and(|l| l.name() == "Markdown");
    let snapshot = buffer.text_snapshot();
    let base_dir = buffer
        .file()
        .and_then(|f| f.as_local())
        .and_then(|f| f.abs_path(cx).parent().map(PathBuf::from));

    let display = editor.display_snapshot(cx);
    let mut cursor_rows: Vec<u32> = editor
        .selections
        .all::<Point>(&display)
        .iter()
        .flat_map(|s| s.start.row..=s.end.row)
        .collect();
    cursor_rows.sort_unstable();
    cursor_rows.dedup();
    let key = is_markdown.then(|| (snapshot.version().clone(), cursor_rows.clone()));
    let Some(state) = editor.addon_mut::<LivePreview>() else {
        return;
    };
    // `None` = nothing applied, so non-markdown editors never get past this.
    if state.key == key {
        return;
    }
    state.key = key;
    let old_blocks = std::mem::take(&mut state.blocks);
    let old_images = std::mem::take(&mut state.images);

    let full = Point::zero()..snapshot.max_point();
    editor.remove_folds_with_type(
        &[full],
        TypeId::of::<EphemeralFold>(),
        false,
        cx,
    );
    editor.clear_highlights(HighlightKey::MarkdownLive, cx);
    if !is_markdown {
        editor.remove_blocks(old_blocks, None, cx);
        return;
    }

    let text = snapshot.text();
    let parsed = parse(&text);
    let on_cursor = |offset: usize| cursor_rows.binary_search(&snapshot.offset_to_point(offset).row).is_ok();
    let to_point = |range: &Range<usize>| {
        snapshot.offset_to_point(range.start)..snapshot.offset_to_point(range.end)
    };
    let mb = editor.buffer().read(cx).snapshot(cx);
    let anchor = |offset: usize| mb.anchor_before(snapshot.offset_to_point(offset));

    let weak = cx.weak_entity();
    let creases = parsed
        .folds
        .into_iter()
        .filter(|(range, _)| !on_cursor(range.start))
        .map(|(range, kind)| {
            let weak = weak.clone();
            let render: Arc<dyn Send + Sync + Fn(FoldId, Range<Anchor>, &mut App) -> AnyElement> =
                match kind {
                    Kind::Hide => Arc::new(|_, _, _| gpui::Empty.into_any_element()),
                    Kind::Bullet => Arc::new(|fold_id, _, cx| {
                        FoldPlaceholder::fold_element(fold_id, cx)
                            .text_color(cx.theme().colors().text_accent)
                            .child("•")
                            .into_any()
                    }),
                    Kind::Task(checked) => Arc::new(move |fold_id, range, cx| {
                        let weak = weak.clone();
                        FoldPlaceholder::fold_element(fold_id, cx)
                            .cursor_pointer()
                            .text_color(cx.theme().colors().text_accent)
                            .child(if checked { "☑" } else { "☐" })
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(move |_, _, cx| {
                                let new = if checked { "[ ]" } else { "[x]" };
                                weak.update(cx, |editor, cx| {
                                    editor.edit([(range.clone(), new)], cx)
                                })
                                .ok();
                            })
                            .into_any()
                    }),
                };
            Crease::simple(
                to_point(&range),
                FoldPlaceholder {
                    render,
                    constrain_width: false,
                    merge_adjacent: false,
                    type_tag: Some(TypeId::of::<EphemeralFold>()),
                    collapsed_text: None,
                },
            )
        })
        .collect::<Vec<_>>();

    let headings = parsed
        .headings
        .iter()
        .map(|r| anchor(r.start)..anchor(r.end))
        .collect();
    let images: Vec<(u32, String)> = parsed
        .images
        .into_iter()
        .filter(|(offset, _)| !on_cursor(*offset))
        .map(|(offset, src)| (snapshot.offset_to_point(offset).row, src))
        .collect();
    let image_blocks = (images != old_images).then(|| {
        images
            .iter()
            .map(|(row, src)| {
                // ImageSource isn't Send/Sync, so capture the plain uri/path and convert at render.
                let source: Result<String, PathBuf> = if src.contains("://") {
                    Ok(src.clone())
                } else {
                    Err(base_dir.clone().unwrap_or_default().join(src))
                };
                BlockProperties {
                    placement: BlockPlacement::Below(mb.anchor_before(Point::new(*row, 0))),
                    height: Some(IMAGE_LINES),
                    style: BlockStyle::Fixed,
                    priority: 0,
                    render: Arc::new(move |bcx| {
                        div()
                            .ml(bcx.anchor_x)
                            .child(
                                img(match &source {
                                    Ok(uri) => ImageSource::from(uri.clone()),
                                    Err(path) => ImageSource::from(path.clone()),
                                })
                                    .h(bcx.line_height * IMAGE_LINES as f32)
                                    .max_w(bcx.max_width - bcx.anchor_x),
                            )
                            .into_any_element()
                    }),
                }
            })
            .collect::<Vec<_>>()
    });

    editor.fold_creases(creases, false, window, cx);
    editor.highlight_text(
        HighlightKey::MarkdownLive,
        headings,
        HighlightStyle {
            color: Some(cx.theme().colors().text_accent),
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        },
        cx,
    );
    let blocks = match image_blocks {
        Some(new_blocks) => {
            editor.remove_blocks(old_blocks, None, cx);
            editor.insert_blocks(new_blocks, None, cx).into_iter().collect()
        }
        None => old_blocks,
    };
    if let Some(state) = editor.addon_mut::<LivePreview>() {
        state.images = images;
        state.blocks = blocks;
    }
}

#[derive(Debug, PartialEq)]
enum Kind {
    Hide,
    Bullet,
    Task(bool),
}

#[derive(Default, Debug)]
struct Parsed {
    folds: Vec<(Range<usize>, Kind)>,
    headings: Vec<Range<usize>>,
    images: Vec<(usize, String)>,
}

fn parse(text: &str) -> Parsed {
    let mut out = Parsed::default();
    // (outer range, union of child ranges, hide delimiters around children?)
    let mut stack: Vec<(Range<usize>, Option<Range<usize>>, bool)> = Vec::new();
    let hide = |out: &mut Parsed, r: Range<usize>, kind: Kind| {
        // Never fold a newline: keep every hidden span on one line.
        let end = text[r.clone()].find('\n').map_or(r.end, |i| r.start + i);
        if end > r.start {
            out.folds.push((r.start..end, kind));
        }
    };
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_WIKILINKS;
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        if !matches!(event, Event::End(_)) {
            if let Some((_, inner, _)) = stack.last_mut() {
                *inner = Some(match inner.take() {
                    Some(r) => r.start.min(range.start)..r.end.max(range.end),
                    None => range.clone(),
                });
            }
        }
        match event {
            Event::Start(tag) => {
                match &tag {
                    Tag::Heading { .. } => out.headings.push(range.clone()),
                    Tag::Image { dest_url, .. } => {
                        hide(&mut out, range.clone(), Kind::Hide);
                        out.images.push((range.start, dest_url.to_string()));
                    }
                    Tag::Item if matches!(text.as_bytes().get(range.start), Some(b'-' | b'*' | b'+')) => {
                        hide(&mut out, range.start..range.start + 1, Kind::Bullet);
                    }
                    _ => {}
                }
                let delimited = matches!(
                    tag,
                    Tag::Heading { .. }
                        | Tag::Emphasis
                        | Tag::Strong
                        | Tag::Strikethrough
                        | Tag::Link { .. }
                );
                stack.push((range, None, delimited));
            }
            Event::End(_) => {
                if let Some((outer, Some(inner), true)) = stack.pop() {
                    hide(&mut out, outer.start..inner.start, Kind::Hide);
                    hide(&mut out, inner.end..outer.end, Kind::Hide);
                }
            }
            Event::Code(_) => {
                let s = &text[range.clone()];
                let lead = s.len() - s.trim_start_matches('`').len();
                let trail = s.len() - s.trim_end_matches('`').len();
                hide(&mut out, range.start..range.start + lead, Kind::Hide);
                hide(&mut out, range.end - trail..range.end, Kind::Hide);
            }
            Event::TaskListMarker(checked) => {
                // `- [ ] x`: hide "- " entirely and show only the checkbox.
                if let Some((r, kind)) = out.folds.last_mut() {
                    if *kind == Kind::Bullet && r.start < range.start {
                        *r = r.start..range.start;
                        *kind = Kind::Hide;
                    }
                }
                hide(&mut out, range, Kind::Task(checked));
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hidden(text: &str) -> String {
        let mut folds = parse(text).folds;
        folds.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
        let mut s = text.to_string();
        for (r, kind) in &folds {
            let with = match kind {
                Kind::Hide => "",
                Kind::Bullet => "•",
                Kind::Task(true) => "☑",
                Kind::Task(false) => "☐",
            };
            s.replace_range(r.clone(), with);
        }
        s
    }

    #[test]
    fn test_parse() {
        assert_eq!(hidden("# Title\n"), "Title\n");
        assert_eq!(hidden("a **b** *c* ~~d~~ `e`\n"), "a b c d e\n");
        assert_eq!(hidden("[text](http://x.y) [[Page]]\n"), "text Page\n");
        assert_eq!(hidden("- one\n- [ ] two\n- [x] three\n"), "• one\n☐ two\n☑ three\n");
        let p = parse("![alt](img.png)\n");
        assert_eq!(p.images, vec![(0, "img.png".to_string())]);
        assert_eq!(hidden("![alt](img.png)\n"), "\n");
    }
}
