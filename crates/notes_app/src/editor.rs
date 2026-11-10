//! Live-preview markdown view: the caret line is raw and editable, every other line is rendered.

use std::{ops::Range, path::PathBuf};

use anyhow::Context as _;
use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ClipboardItem, Context, CursorStyle, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, Font, GlobalElementId,
    Hsla, InteractiveText, KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollHandle, SharedString, Style, StyledText, TextRun,
    UTF16Selection, UnderlineStyle, Window, WrappedLine, actions, anchored, deferred, div, fill,
    font, img, point, prelude::*, px, relative, rgb, rgba, size, svg,
};

use crate::highlight;

#[path = "menu.rs"]
mod menu;
use crate::markdown::{self, Block, Kind, MONO, Palette};
pub use menu::icon_asset;

actions!(
    notes_editor,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        Home,
        End,
        SelectLeft,
        SelectRight,
        SelectHome,
        SelectEnd,
        SelectAll,
        Enter,
        Tab,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        Bold,
        Italic,
        InsertLink,
        CloseMenu,
    ]
);

const CONTEXT: &str = "NotesEditor";
const UI: &str = "Segoe UI";
const BASE: f32 = 16.;
const LH: f32 = 26.;
const HEADING: [f32; 6] = [2.0, 1.6, 1.37, 1.25, 1.12, 1.0];
const UNDO_CAP: usize = 200;

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn palette() -> Palette {
    Palette {
        accent: c(0x7f6df2),
        dim: c(0x999999),
        code_bg: c(0x2a2a2a),
    }
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("left", Left, Some(CONTEXT)),
        KeyBinding::new("right", Right, Some(CONTEXT)),
        KeyBinding::new("up", Up, Some(CONTEXT)),
        KeyBinding::new("down", Down, Some(CONTEXT)),
        KeyBinding::new("home", Home, Some(CONTEXT)),
        KeyBinding::new("end", End, Some(CONTEXT)),
        KeyBinding::new("shift-left", SelectLeft, Some(CONTEXT)),
        KeyBinding::new("shift-right", SelectRight, Some(CONTEXT)),
        KeyBinding::new("shift-home", SelectHome, Some(CONTEXT)),
        KeyBinding::new("shift-end", SelectEnd, Some(CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("enter", Enter, Some(CONTEXT)),
        KeyBinding::new("tab", Tab, Some(CONTEXT)),
        KeyBinding::new("ctrl-c", Copy, Some(CONTEXT)),
        KeyBinding::new("ctrl-x", Cut, Some(CONTEXT)),
        KeyBinding::new("ctrl-v", Paste, Some(CONTEXT)),
        KeyBinding::new("ctrl-z", Undo, Some(CONTEXT)),
        KeyBinding::new("ctrl-y", Redo, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-z", Redo, Some(CONTEXT)),
        KeyBinding::new("ctrl-b", Bold, Some(CONTEXT)),
        KeyBinding::new("ctrl-i", Italic, Some(CONTEXT)),
        KeyBinding::new("ctrl-k", InsertLink, Some(CONTEXT)),
        KeyBinding::new("escape", CloseMenu, Some(CONTEXT)),
    ]);
}

type Snap = (Vec<String>, usize, usize);

pub struct Editor {
    focus: FocusHandle,
    lines: Vec<String>,
    row: usize,
    /// Byte range within `lines[row]`.
    sel: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    /// ctrl-a: the whole document is selected.
    all: bool,
    // ponytail: whole-document snapshots; fine for notes, use an edit log if files get huge.
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    last_typing: bool,
    path: Option<PathBuf>,
    dirty: bool,
    crlf: bool,
    layout: Option<WrappedLine>,
    bounds: Option<Bounds<Pixels>>,
    lh: Pixels,
    selecting: bool,
    scroll: ScrollHandle,
    reveal: bool,
    title: String,
    menu: Option<menu::Menu>,
    /// Bytes of the caret line's marker (`- `, `## `, …) that the last paint drew as a widget.
    drawn_head: usize,
}

impl Editor {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            lines: vec![String::new()],
            row: 0,
            sel: 0..0,
            reversed: false,
            marked: None,
            all: false,
            undo: Vec::new(),
            redo: Vec::new(),
            last_typing: false,
            path: None,
            dirty: false,
            crlf: false,
            layout: None,
            bounds: None,
            lh: px(LH),
            selecting: false,
            scroll: ScrollHandle::new(),
            reveal: false,
            title: String::new(),
            menu: None,
            drawn_head: 0,
        }
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn open(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let text = match &path {
            Some(p) => {
                std::fs::read_to_string(p).with_context(|| format!("open {}", p.display()))?
            }
            None => String::new(),
        };
        self.crlf = text.contains("\r\n");
        self.lines = text
            .split('\n')
            .map(|l| l.trim_end_matches('\r').to_string())
            .collect();
        self.path = path;
        self.dirty = false;
        self.undo.clear();
        self.redo.clear();
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.set_caret(0, 0, cx);
        Ok(())
    }

    pub fn save_to(&mut self, path: PathBuf, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let text = self.lines.join(if self.crlf { "\r\n" } else { "\n" });
        std::fs::write(&path, text).with_context(|| format!("save {}", path.display()))?;
        self.path = Some(path);
        self.dirty = false;
        cx.notify();
        Ok(())
    }

    fn line(&self) -> &str {
        &self.lines[self.row]
    }

    fn cursor(&self) -> usize {
        if self.reversed {
            self.sel.start
        } else {
            self.sel.end
        }
    }

    /// Length of `row`'s block marker that is shown as a widget (bullet, number, checkbox,
    /// heading size, quote bar) while that line is edited. Zero inside code fences.
    fn head_of(&self, row: usize) -> usize {
        let line = &self.lines[row];
        let fences = self.lines[..row]
            .iter()
            .filter(|l| markdown::is_fence(l))
            .count();
        if fences % 2 == 1 || markdown::is_fence(line) {
            0
        } else {
            markdown::marker_len(line)
        }
    }

    fn head(&self) -> usize {
        self.head_of(self.row)
    }

    fn set_caret(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        self.row = row.min(self.lines.len() - 1);
        let head = self.head_of(self.row);
        let line = &self.lines[self.row];
        let mut col = col.min(line.len()).max(head);
        while !line.is_char_boundary(col) {
            col -= 1;
        }
        self.sel = col..col;
        self.reversed = false;
        self.marked = None;
        self.all = false;
        self.last_typing = false;
        self.reveal = true;
        cx.notify();
    }

    fn select_to(&mut self, col: usize, cx: &mut Context<Self>) {
        self.all = false;
        let col = col.max(self.head());
        if self.reversed {
            self.sel.start = col
        } else {
            self.sel.end = col
        }
        if self.sel.end < self.sel.start {
            self.reversed = !self.reversed;
            self.sel = self.sel.end..self.sel.start;
        }
        cx.notify();
    }

    fn prev(&self, col: usize) -> usize {
        self.line()[..col]
            .char_indices()
            .last()
            .map_or(0, |(i, _)| i)
    }

    fn next(&self, col: usize) -> usize {
        self.line()[col..]
            .chars()
            .next()
            .map_or(col, |ch| col + ch.len_utf8())
    }

    /// Moves vertically keeping the column in chars.
    fn vertical(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(row) = self
            .row
            .checked_add_signed(delta)
            .filter(|&r| r < self.lines.len())
        else {
            return;
        };
        let head = self.head();
        let chars = self.line()[head..self.cursor().max(head)].chars().count();
        let target_head = self.head_of(row);
        let line = &self.lines[row][target_head..];
        let col = target_head
            + line
                .char_indices()
                .nth(chars)
                .map_or(line.len(), |(i, _)| i);
        self.set_caret(row, col, cx);
    }

    fn checkpoint(&mut self, typing: bool) {
        if !(typing && self.last_typing) {
            self.undo
                .push((self.lines.clone(), self.row, self.cursor()));
            if self.undo.len() > UNDO_CAP {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last_typing = typing;
        self.dirty = true;
    }

    /// Replaces `range` of the caret line (or everything when `all`) with `text`, which may
    /// contain newlines. Returns the byte offset where `text` starts in the caret line.
    fn replace(
        &mut self,
        mut range: Range<usize>,
        text: &str,
        typing: bool,
        cx: &mut Context<Self>,
    ) -> usize {
        self.checkpoint(typing);
        if self.all {
            self.lines = vec![String::new()];
            self.row = 0;
            range = 0..0;
            self.all = false;
        }
        let line = &self.lines[self.row];
        let head = line[..range.start].to_string();
        let tail = line[range.end..].to_string();
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut parts = text.split('\n');
        let mut new = vec![head + parts.next().unwrap_or("")];
        new.extend(parts.map(String::from));
        let last = new.len() - 1;
        let col = new[last].len();
        new[last].push_str(&tail);
        self.lines.splice(self.row..=self.row, new);
        self.row += last;
        self.sel = col..col;
        self.reversed = false;
        self.marked = None;
        self.reveal = true;
        cx.notify();
        range.start
    }

    fn delete_sel_or(&mut self, other: Range<usize>, cx: &mut Context<Self>) {
        let range = if self.sel.is_empty() {
            other
        } else {
            self.sel.clone()
        };
        self.replace(range, "", false, cx);
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        let col = self.cursor();
        let head = self.head();
        if !self.all && self.sel.is_empty() && head > 0 && col == head {
            self.replace(0..head, "", false, cx);
        } else if self.all || !self.sel.is_empty() || col > 0 {
            self.delete_sel_or(self.prev(col)..col, cx);
        } else if self.row > 0 {
            self.checkpoint(false);
            let cur = self.lines.remove(self.row);
            let prev = &mut self.lines[self.row - 1];
            let col = prev.len();
            prev.push_str(&cur);
            self.set_caret(self.row - 1, col, cx);
        }
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        let col = self.cursor();
        if self.all || !self.sel.is_empty() || col < self.line().len() {
            self.delete_sel_or(col..self.next(col), cx);
        } else if self.row + 1 < self.lines.len() {
            self.checkpoint(false);
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
            self.set_caret(self.row, col, cx);
        }
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let col = self.cursor();
        if !self.sel.is_empty() {
            self.set_caret(self.row, self.sel.start, cx);
        } else if col > self.head() {
            self.set_caret(self.row, self.prev(col), cx);
        } else if self.row > 0 {
            self.set_caret(self.row - 1, usize::MAX, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let col = self.cursor();
        if !self.sel.is_empty() {
            self.set_caret(self.row, self.sel.end, cx);
        } else if col < self.line().len() {
            self.set_caret(self.row, self.next(col), cx);
        } else if self.row + 1 < self.lines.len() {
            self.set_caret(self.row + 1, 0, cx);
        }
    }

    fn enter(&mut self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        match markdown::list_prefix(self.line()) {
            // Enter on an empty list item ends the list.
            Some((own, _)) if !self.all && own == self.line().len() => {
                let len = self.line().len();
                self.replace(0..len, "", false, cx);
            }
            Some((_, next)) => {
                self.replace(self.sel.clone(), &format!("\n{next}"), false, cx);
            }
            None => {
                self.replace(self.sel.clone(), "\n", false, cx);
            }
        }
    }

    fn selected_text(&self) -> String {
        if self.all {
            self.lines.join("\n")
        } else {
            self.line()[self.sel.clone()].to_string()
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.all || !self.sel.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.selected_text()));
        }
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if self.all || !self.sel.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.selected_text()));
            self.replace(self.sel.clone(), "", false, cx);
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace(self.sel.clone(), &text, false, cx);
        }
    }

    fn restore(&mut self, from_undo: bool, cx: &mut Context<Self>) {
        let now = (self.row, self.cursor());
        let src = if from_undo {
            &mut self.undo
        } else {
            &mut self.redo
        };
        let Some((lines, row, col)) = src.pop() else {
            return;
        };
        let old = std::mem::replace(&mut self.lines, lines);
        let dst = if from_undo {
            &mut self.redo
        } else {
            &mut self.undo
        };
        dst.push((old, now.0, now.1));
        self.dirty = true;
        self.set_caret(row, col, cx);
    }

    fn toggle_task(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(new) = markdown::toggle_task(&self.lines[ix]) {
            self.checkpoint(false);
            self.lines[ix] = new;
            cx.notify();
        }
    }

    fn index_at(&self, pos: Point<Pixels>) -> usize {
        let (Some(layout), Some(b)) = (&self.layout, self.bounds) else {
            return self.line().len();
        };
        let local = point(pos.x - b.left(), (pos.y - b.top()).max(px(0.)));
        let i = (self.drawn_head
            + layout
                .closest_index_for_position(local, self.lh)
                .unwrap_or_else(|i| i))
        .min(self.line().len());
        if self.line().is_char_boundary(i) {
            i
        } else {
            self.line().len()
        }
    }

    fn click(
        &mut self,
        ix: usize,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        self.menu = None;
        if ev.modifiers.control {
            return; // ctrl-click is for links
        }
        let ix = ix.min(self.lines.len() - 1);
        if ix == self.row {
            let col = self.index_at(ev.position);
            if ev.modifiers.shift {
                self.select_to(col, cx);
            } else {
                self.set_caret(ix, col, cx);
            }
        } else {
            // ponytail: rendered lines put the caret at line end; map x through the rendered layout for exact columns.
            self.set_caret(ix, usize::MAX, cx);
        }
        self.selecting = true;
    }

    fn utf16_to_utf8(&self, offset: usize) -> usize {
        let mut utf16 = 0;
        for (i, ch) in self.line().char_indices() {
            if utf16 >= offset {
                return i;
            }
            utf16 += ch.len_utf16();
        }
        self.line().len()
    }

    fn utf8_to_utf16(&self, offset: usize) -> usize {
        self.line()[..offset.min(self.line().len())]
            .encode_utf16()
            .count()
    }

    fn range_from_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.utf16_to_utf8(r.start)..self.utf16_to_utf8(r.end)
    }

    fn range_to_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.utf8_to_utf16(r.start)..self.utf8_to_utf16(r.end)
    }

    // ---- rendering ----

    fn text(&self, id: usize, src: &str, base: Font, color: Hsla) -> AnyElement {
        let inl = markdown::inline(src, &base, color, &palette());
        if inl.text.is_empty() {
            return div().into_any_element();
        }
        let styled = StyledText::new(inl.text).with_runs(inl.runs);
        if inl.links.is_empty() {
            return styled.into_any_element();
        }
        let (ranges, urls): (Vec<_>, Vec<_>) = inl.links.into_iter().unzip();
        InteractiveText::new(("link", id), styled)
            .on_click(ranges, move |i, window, cx| {
                if window.modifiers().control {
                    cx.open_url(&urls[i]);
                }
            })
            .into_any_element()
    }

    /// Inline text of a line, or the editable caret line in its place.
    fn body(
        &self,
        ix: usize,
        rest: &str,
        base: Font,
        color: Hsla,
        editing: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if editing {
            CaretLine {
                editor: cx.entity(),
                font: base,
                start: self.head(),
            }
            .into_any_element()
        } else {
            self.text(ix, rest, base, color)
        }
    }

    /// Bullet / number / checkbox column, with room before the text.
    fn list_marker(
        &self,
        ix: usize,
        marker: String,
        task: Option<bool>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette();
        let slot = div().w(px(28.)).h(px(LH)).flex_none().flex().items_center();
        match task {
            Some(done) => slot
                .child(
                    div()
                        .size(px(16.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(if done { p.accent } else { c(0x8a8a8a) })
                        .when(done, |d| {
                            d.bg(p.accent).child(
                                svg()
                                    .path("icons/notes/check.svg")
                                    .w(px(12.))
                                    .h(px(12.))
                                    .text_color(c(0xffffff)),
                            )
                        })
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.toggle_task(ix, cx);
                            }),
                        ),
                )
                .into_any_element(),
            None if marker == "•" => slot
                .justify_center()
                .child(div().size(px(6.)).rounded_full().bg(p.dim))
                .into_any_element(),
            None => slot.text_color(p.dim).child(marker).into_any_element(),
        }
    }

    fn rendered(&self, ix: usize, editing: bool, cx: &mut Context<Self>) -> AnyElement {
        let p = palette();
        let text = c(0xdcddde);
        let base = font(UI);
        match markdown::classify(&self.lines[ix]) {
            Kind::Heading(level, rest) => {
                let size = BASE * HEADING[level - 1];
                div()
                    .pt(px(size * 0.3))
                    .text_size(px(size))
                    .line_height(px(size * 1.3))
                    .child(self.body(ix, rest, base.bold(), text, editing, cx))
                    .into_any_element()
            }
            Kind::Quote(rest) => div()
                .border_l_2()
                .border_color(p.accent)
                .pl_4()
                .child(self.body(ix, rest, base, text, editing, cx))
                .into_any_element(),
            Kind::List {
                indent,
                marker,
                task,
                rest,
            } => {
                let color = if task == Some(true) { p.dim } else { text };
                div()
                    .flex()
                    .pl(px(indent as f32 * 10.))
                    .child(self.list_marker(ix, marker, task, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .pl(px(4.))
                            .child(self.body(ix, rest, base, color, editing, cx)),
                    )
                    .into_any_element()
            }
            // Everything else is edited as plain source.
            _ if editing => self.raw(ix, false, cx),
            Kind::Blank => div().into_any_element(),
            Kind::Rule => div()
                .h(px(LH))
                .flex()
                .items_center()
                .child(div().w_full().h_px().bg(c(0x444444)))
                .into_any_element(),
            Kind::Image(url) if !url.contains("://") => {
                let dir = self
                    .path
                    .as_ref()
                    .and_then(|p| p.parent())
                    .map(PathBuf::from);
                let path = dir.unwrap_or_default().join(url);
                div()
                    .py_1()
                    .child(img(path).max_w_full())
                    .into_any_element()
            }
            // ponytail: remote images need an HTTP client; shown as their source line.
            Kind::Image(_) => self.text(ix, &self.lines[ix], base, p.dim),
            Kind::Text(t) => self.text(ix, t, base, text),
        }
    }

    /// Last body line of the fence block `r` (exclusive), i.e. without the closing ```.
    fn fence_end(&self, r: &Range<usize>) -> usize {
        if r.len() > 1 && markdown::is_fence(&self.lines[r.end - 1]) {
            r.end - 1
        } else {
            r.end
        }
    }

    /// Syntax-coloured text runs for every body line of the fence block `r`.
    fn fence_runs(&self, r: &Range<usize>) -> Vec<Vec<TextRun>> {
        let info = markdown::fence_info(&self.lines[r.start]);
        let body = &self.lines[r.start + 1..self.fence_end(r)];
        let code = body.join("\n");
        let spans = highlight::highlight(info, &code);
        let mono = font(MONO);
        let run = |len: usize, color: Hsla| TextRun {
            len,
            font: mono.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let plain = c(0xd4d4d4);
        let mut offset = 0;
        body.iter()
            .map(|line| {
                let (s, e) = (offset, offset + line.len());
                offset = e + 1;
                let mut runs = Vec::new();
                let mut pos = s;
                for (range, color) in spans.iter().filter(|(r, _)| r.end > s && r.start < e) {
                    let (a, b) = (range.start.max(s), range.end.min(e));
                    if a > pos {
                        runs.push(run(a - pos, plain));
                    }
                    if b > a.max(pos) {
                        runs.push(run(b - a.max(pos), *color));
                        pos = b;
                    }
                }
                if e > pos {
                    runs.push(run(e - pos, plain));
                }
                runs
            })
            .collect()
    }

    fn code_line(line: &str, runs: Vec<TextRun>) -> AnyElement {
        if line.is_empty() {
            return div().min_h(px(20.)).into_any_element();
        }
        div()
            .min_h(px(20.))
            .child(StyledText::new(line.to_string()).with_runs(runs))
            .into_any_element()
    }

    fn fence(&self, r: Range<usize>) -> AnyElement {
        let info = markdown::fence_info(&self.lines[r.start]).to_string();
        let end = self.fence_end(&r);
        let runs = self.fence_runs(&r);
        div()
            .relative()
            .my_1()
            .p_3()
            .rounded_md()
            .bg(palette().code_bg)
            .font_family(MONO)
            .text_size(px(14.))
            .line_height(px(20.))
            .when(!info.is_empty(), |d| {
                d.child(
                    div()
                        .absolute()
                        .top_1()
                        .right_2()
                        .font_family(UI)
                        .text_size(px(11.))
                        .text_color(palette().dim)
                        .child(info),
                )
            })
            .children(
                self.lines[r.start + 1..end]
                    .iter()
                    .zip(runs)
                    .map(|(l, runs)| Self::code_line(l, runs)),
            )
            .into_any_element()
    }

    fn table(&self, r: Range<usize>) -> AnyElement {
        let border = c(0x444444);
        let sep = r.start + 1;
        div()
            .my_1()
            .flex()
            .flex_col()
            .border_1()
            .border_color(border)
            .children(r.clone().filter(|&i| i != sep).map(|i| {
                let base = if i == r.start {
                    font(UI).bold()
                } else {
                    font(UI)
                };
                div()
                    .flex()
                    .children(markdown::cells(&self.lines[i]).into_iter().enumerate().map(
                        |(j, cell)| {
                            div()
                                .flex_1()
                                .px_2()
                                .py_1()
                                .border_1()
                                .border_color(border)
                                .child(self.text(i * 64 + j, cell, base.clone(), c(0xdcddde)))
                        },
                    ))
            }))
            .into_any_element()
    }

    fn raw(&self, ix: usize, mono: bool, cx: &mut Context<Self>) -> AnyElement {
        let el = div().when(mono, |d| d.font_family(MONO).text_size(px(14.)));
        if ix == self.row {
            el.child(CaretLine {
                editor: cx.entity(),
                font: font(if mono { MONO } else { UI }),
                start: 0,
            })
            .into_any_element()
        } else {
            el.child(self.lines[ix].clone()).into_any_element()
        }
    }

    fn wrap_row(&self, ix: usize, inner: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let gutter = div()
            .w(px(44.))
            .flex_none()
            .pr_3()
            .h(px(LH))
            .flex()
            .items_center()
            .justify_end()
            .font_family(MONO)
            .text_size(px(12.))
            .text_color(if ix == self.row {
                c(0xb5b5b5)
            } else {
                c(0x5a5a5a)
            })
            .child((ix + 1).to_string());
        div()
            .w_full()
            .max_w(px(800.))
            .pr_6()
            .flex()
            .items_start()
            .flex_shrink_0()
            .min_h(px(LH))
            .when(self.all, |d| d.bg(rgba(0x7f6df240)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    this.click(ix, ev, window, cx)
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    let ix = ix.min(this.lines.len() - 1);
                    if ix != this.row {
                        this.set_caret(ix, usize::MAX, cx);
                    }
                    cx.stop_propagation();
                    this.open_menu(ev.position, cx);
                }),
            )
            .child(gutter)
            .child(div().flex_1().min_w_0().child(inner))
            .into_any_element()
    }

    fn update_title(&mut self, window: &mut Window) {
        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map_or("Untitled".into(), |n| n.to_string_lossy());
        let dirty = if self.dirty { " •" } else { "" };
        let title = format!("{name}{dirty} - Notes");
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_title(window);
        // ponytail: every line is laid out each frame; switch to gpui::list() if multi-thousand-line files lag.
        let mut rows = Vec::new();
        let mut caret_child = 0;
        for block in markdown::blocks(&self.lines) {
            match block {
                Block::Line(i) => {
                    if i == self.row {
                        caret_child = rows.len();
                    }
                    let inner = self.rendered(i, i == self.row, cx);
                    rows.push(self.wrap_row(i, inner, cx));
                }
                Block::Fence(r) if r.contains(&self.row) => {
                    let runs = self.fence_runs(&r);
                    let body = r.start + 1..self.fence_end(&r);
                    for i in r.clone() {
                        if i == self.row {
                            caret_child = rows.len();
                        }
                        let inner = if i != self.row && body.contains(&i) {
                            div()
                                .font_family(MONO)
                                .text_size(px(14.))
                                .child(Self::code_line(
                                    &self.lines[i],
                                    runs[i - body.start].clone(),
                                ))
                                .into_any_element()
                        } else {
                            self.raw(i, true, cx)
                        };
                        rows.push(self.wrap_row(i, inner, cx));
                    }
                }
                Block::Table(r) if r.contains(&self.row) => {
                    for i in r {
                        if i == self.row {
                            caret_child = rows.len();
                        }
                        let inner = self.raw(i, false, cx);
                        rows.push(self.wrap_row(i, inner, cx));
                    }
                }
                Block::Fence(r) => {
                    let start = r.start;
                    let inner = self.fence(r);
                    rows.push(self.wrap_row(start, inner, cx));
                }
                Block::Table(r) => {
                    let start = r.start;
                    let inner = self.table(r);
                    rows.push(self.wrap_row(start, inner, cx));
                }
            }
        }
        if std::mem::take(&mut self.reveal) {
            self.scroll.scroll_to_item(caret_child);
        }
        let last = self.lines.len() - 1;
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .bg(c(0x1e1e1e))
            .text_color(c(0xdcddde))
            .font_family(UI)
            .text_size(px(BASE))
            .line_height(px(LH))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(|this, _: &Up, _, cx| this.vertical(-1, cx)))
            .on_action(cx.listener(|this, _: &Down, _, cx| this.vertical(1, cx)))
            .on_action(cx.listener(|this, _: &Home, _, cx| this.set_caret(this.row, 0, cx)))
            .on_action(cx.listener(|this, _: &End, _, cx| this.set_caret(this.row, usize::MAX, cx)))
            .on_action(cx.listener(|this, _: &SelectLeft, _, cx| {
                this.select_to(this.prev(this.cursor()), cx)
            }))
            .on_action(cx.listener(|this, _: &SelectRight, _, cx| {
                this.select_to(this.next(this.cursor()), cx)
            }))
            .on_action(cx.listener(|this, _: &SelectHome, _, cx| this.select_to(0, cx)))
            .on_action(
                cx.listener(|this, _: &SelectEnd, _, cx| this.select_to(this.line().len(), cx)),
            )
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.all = true;
                cx.notify();
            }))
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(|this, _: &Tab, _, cx| {
                this.replace(this.sel.clone(), "    ", false, cx);
            }))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(|this, _: &Undo, _, cx| this.restore(true, cx)))
            .on_action(cx.listener(|this, _: &Redo, _, cx| this.restore(false, cx)))
            .on_action(cx.listener(|this, _: &Bold, _, cx| this.wrap("**", "**", cx)))
            .on_action(cx.listener(|this, _: &Italic, _, cx| this.wrap("*", "*", cx)))
            .on_action(cx.listener(|this, _: &InsertLink, window, cx| {
                this.run_cmd(menu::Cmd::ExternalLink, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CloseMenu, _, cx| this.close_menu(cx)))
            // Any click outside the menu closes it (the menu itself occludes the page).
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| this.close_menu(cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    this.open_menu(ev.position, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if this.selecting && ev.pressed_button == Some(MouseButton::Left) {
                    let col = this.index_at(ev.position);
                    this.select_to(col, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| this.selecting = false),
            )
            .child(
                div()
                    .id("notes-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .flex()
                    .flex_col()
                    .items_center()
                    .py(px(32.))
                    .children(rows)
                    .child(
                        // Clicking below the text puts the caret at the end.
                        div().w_full().h(px(200.)).flex_shrink_0().on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                window.focus(&this.focus, cx);
                                this.set_caret(last, usize::MAX, cx);
                            }),
                        ),
                    ),
            )
            .children(self.render_menu(cx))
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.line()[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.sel),
            reversed: self.reversed,
        })
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sel = self.range_from_utf16(&range_utf16);
        self.reversed = false;
        cx.notify();
    }

    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.line().encode_utf16().count())
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.range_from_utf16(&r))
            .or(self.marked.clone())
            .unwrap_or(self.sel.clone());
        self.replace(range, text, true, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.range_from_utf16(&r))
            .or(self.marked.clone())
            .unwrap_or(self.sel.clone());
        let start = self.replace(range, new_text, true, cx);
        if new_text.is_empty() || new_text.contains('\n') {
            return;
        }
        self.marked = Some(start..start + new_text.len());
        if let Some(sel) = new_selected_range_utf16 {
            let base = self.utf8_to_utf16(start);
            let len = new_text.encode_utf16().count();
            self.sel = self.range_from_utf16(&(base + sel.start.min(len)..base + sel.end.min(len)));
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let h = self.drawn_head;
        let a = layout.position_for_index(range.start.saturating_sub(h), self.lh)?;
        let b = layout
            .position_for_index(range.end.saturating_sub(h), self.lh)
            .unwrap_or(a);
        let width = if b.y == a.y { b.x - a.x } else { px(0.) };
        Some(Bounds::new(
            point(bounds.left() + a.x, bounds.top() + a.y),
            size(width.max(px(1.)), self.lh),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.bounds?;
        Some(self.utf8_to_utf16(self.index_at(point)))
    }
}

/// The editable caret line: soft-wrapped text, selection, caret, IME registration.
struct CaretLine {
    editor: Entity<Editor>,
    font: Font,
    /// Byte offset in the line where the drawn text starts (after a hidden marker).
    start: usize,
}

impl CaretLine {
    fn shape_input(&self, window: &Window, cx: &App) -> (SharedString, Vec<TextRun>) {
        let editor = self.editor.read(cx);
        let h = self.start.min(editor.line().len());
        let text: SharedString = editor.line()[h..].to_string().into();
        let style = window.text_style();
        let run = TextRun {
            len: text.len(),
            font: self.font.clone(),
            color: style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let marked = editor
            .marked
            .clone()
            .map(|m| m.start.saturating_sub(h)..m.end.saturating_sub(h));
        let runs = match &marked {
            Some(m) => [
                TextRun {
                    len: m.start,
                    ..run.clone()
                },
                TextRun {
                    len: m.end - m.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: text.len() - m.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|r| r.len > 0)
            .collect(),
            None => vec![run],
        };
        (text, runs)
    }
}

impl IntoElement for CaretLine {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for CaretLine {
    type RequestLayoutState = ();
    type PrepaintState = Option<WrappedLine>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let (text, runs) = self.shape_input(window, cx);
        let font_size = window.text_style().font_size.to_pixels(window.rem_size());
        let lh = window.line_height();
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let id = window.request_measured_layout(style, move |known, available, window, _| {
            let width = known.width.or(match available.width {
                AvailableSpace::Definite(x) => Some(x),
                _ => None,
            });
            let height = window
                .text_system()
                .shape_text(text.clone(), font_size, &runs, width, None)
                .ok()
                .and_then(|lines| lines.into_iter().next())
                .map_or(lh, |line| line.size(lh).height.max(lh));
            size(width.unwrap_or(px(0.)), height)
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<WrappedLine> {
        let (text, runs) = self.shape_input(window, cx);
        let font_size = window.text_style().font_size.to_pixels(window.rem_size());
        window
            .text_system()
            .shape_text(text, font_size, &runs, Some(bounds.size.width), None)
            .ok()
            .and_then(|lines| lines.into_iter().next())
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        layout: &mut Option<WrappedLine>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.editor.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
        let lh = window.line_height();
        let Some(line) = layout.take() else {
            return;
        };
        let editor = self.editor.read(cx);
        let h = self.start;
        let (sel, cursor, all) = (
            editor.sel.start.saturating_sub(h)..editor.sel.end.saturating_sub(h),
            editor.cursor().saturating_sub(h),
            editor.all,
        );
        let pos = |i: usize| {
            let p = line.position_for_index(i, lh).unwrap_or_default();
            point(bounds.left() + p.x, bounds.top() + p.y)
        };
        if !sel.is_empty() && !all {
            let color = rgba(0x7f6df260);
            let (a, b) = (pos(sel.start), pos(sel.end));
            if a.y == b.y {
                window.paint_quad(fill(Bounds::from_corners(a, point(b.x, b.y + lh)), color));
            } else {
                let right = bounds.right();
                window.paint_quad(fill(Bounds::from_corners(a, point(right, a.y + lh)), color));
                if b.y > a.y + lh {
                    window.paint_quad(fill(
                        Bounds::from_corners(point(bounds.left(), a.y + lh), point(right, b.y)),
                        color,
                    ));
                }
                window.paint_quad(fill(
                    Bounds::from_corners(point(bounds.left(), b.y), point(b.x, b.y + lh)),
                    color,
                ));
            }
        }
        line.paint(
            bounds.origin,
            lh,
            gpui::TextAlign::Left,
            Some(bounds),
            window,
            cx,
        )
        .ok();
        if focus.is_focused(window) && sel.is_empty() {
            let p = pos(cursor);
            window.paint_quad(fill(Bounds::new(p, size(px(2.), lh)), c(0x7f6df2)));
        }
        self.editor.update(cx, |editor, _| {
            editor.layout = Some(line);
            editor.bounds = Some(bounds);
            editor.lh = lh;
            editor.drawn_head = h;
        });
    }
}
