//! Obsidian-style right-click menu (Add link, Format ›, Paragraph ›, Insert ›, clipboard, …)
//! and the editing commands behind it.

use super::*;

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        /// Menu icons (Tabler Icons, MIT; `assets/icons/notes`), served to gpui as assets.
        pub fn icon_asset(path: &str) -> Option<&'static [u8]> {
            let name = path.strip_prefix("icons/notes/")?;
            $(if name == concat!($name, ".svg") {
                return Some(include_bytes!(concat!("../../../assets/icons/notes/", $name, ".svg")));
            })*
            None
        }
    };
}
icons!(
    "link",
    "external-link",
    "brush",
    "pilcrow",
    "row-insert-bottom",
    "cut",
    "copy",
    "clipboard",
    "clipboard-text",
    "select-all",
    "pencil",
    "chevron-right",
    "bold",
    "italic",
    "strikethrough",
    "highlight",
    "code",
    "math-function",
    "message",
    "clear-formatting",
    "list",
    "list-numbers",
    "list-check",
    "h-1",
    "h-2",
    "h-3",
    "h-4",
    "h-5",
    "h-6",
    "text-size",
    "quote",
    "table",
    "info-circle",
    "separator-horizontal",
    "source-code",
    "math",
);

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Sub {
    Format,
    Paragraph,
    Insert,
}

pub(super) struct Menu {
    pub pos: Point<Pixels>,
    pub sub: Option<Sub>,
}

#[derive(Clone, Copy)]
pub(super) enum Cmd {
    Link,
    ExternalLink,
    Wrap(&'static str, &'static str),
    ClearFormat,
    Prefix(&'static str),
    /// Block text to insert, and how many lines up the caret goes afterwards.
    Insert(&'static str, usize),
    Cut,
    Copy,
    Paste,
    SelectAll,
    RenameHeading,
}

struct Item {
    icon: &'static str,
    label: &'static str,
    key: &'static str,
    cmd: Option<Cmd>,
    sub: Option<Sub>,
    enabled: bool,
}

const fn item(icon: &'static str, label: &'static str, key: &'static str, cmd: Cmd) -> Item {
    Item {
        icon,
        label,
        key,
        cmd: Some(cmd),
        sub: None,
        enabled: true,
    }
}

const fn submenu(icon: &'static str, label: &'static str, sub: Sub) -> Item {
    Item {
        icon,
        label,
        key: "",
        cmd: None,
        sub: Some(sub),
        enabled: true,
    }
}

const ITEM_H: f32 = 28.;
const SEP_H: f32 = 9.;
const PAD: f32 = 4.;

fn sub_items(sub: Sub) -> Vec<Vec<Item>> {
    match sub {
        Sub::Format => vec![
            vec![
                item("bold", "Bold", "Ctrl+B", Cmd::Wrap("**", "**")),
                item("italic", "Italic", "Ctrl+I", Cmd::Wrap("*", "*")),
                item("strikethrough", "Strikethrough", "", Cmd::Wrap("~~", "~~")),
                item("highlight", "Highlight", "", Cmd::Wrap("==", "==")),
                item("code", "Code", "", Cmd::Wrap("`", "`")),
                item("math-function", "Math", "", Cmd::Wrap("$", "$")),
                item("message", "Comment", "", Cmd::Wrap("%%", "%%")),
            ],
            vec![item(
                "clear-formatting",
                "Clear formatting",
                "",
                Cmd::ClearFormat,
            )],
        ],
        Sub::Paragraph => vec![
            vec![
                item("list", "Bullet list", "", Cmd::Prefix("- ")),
                item("list-numbers", "Numbered list", "", Cmd::Prefix("1. ")),
                item("list-check", "Task list", "", Cmd::Prefix("- [ ] ")),
            ],
            vec![
                item("h-1", "Heading 1", "", Cmd::Prefix("# ")),
                item("h-2", "Heading 2", "", Cmd::Prefix("## ")),
                item("h-3", "Heading 3", "", Cmd::Prefix("### ")),
                item("h-4", "Heading 4", "", Cmd::Prefix("#### ")),
                item("h-5", "Heading 5", "", Cmd::Prefix("##### ")),
                item("h-6", "Heading 6", "", Cmd::Prefix("###### ")),
                item("text-size", "Body", "", Cmd::Prefix("")),
            ],
            vec![item("quote", "Quote", "", Cmd::Prefix("> "))],
        ],
        Sub::Insert => vec![vec![
            item(
                "table",
                "Table",
                "",
                Cmd::Insert(
                    "| Column 1 | Column 2 |\n| -------- | -------- |\n|          |          |",
                    0,
                ),
            ),
            item(
                "info-circle",
                "Callout",
                "",
                Cmd::Insert("> [!note]\n> ", 0),
            ),
            item(
                "separator-horizontal",
                "Horizontal rule",
                "",
                Cmd::Insert("---", 0),
            ),
            item(
                "source-code",
                "Code block",
                "",
                Cmd::Insert("```\n\n```", 1),
            ),
            item("math", "Math block", "", Cmd::Insert("$$\n\n$$", 1)),
        ]],
    }
}

/// Length of a line's block marker (`## `, `- [ ] `, `1. `, `> `) including its indent.
fn block_prefix_len(line: &str) -> usize {
    let t = line.trim_start();
    let indent = line.len() - t.len();
    let hashes = t.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
        return indent + hashes + 1;
    }
    if t.starts_with("> ") {
        return indent + 2;
    }
    markdown::list_prefix(line).map_or(indent, |(own, _)| own)
}

impl Editor {
    pub(super) fn open_menu(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        self.menu = Some(Menu { pos, sub: None });
        cx.notify();
    }

    pub(super) fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn run_cmd(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        match cmd {
            Cmd::Link => self.wrap("[[", "]]", cx),
            Cmd::ExternalLink => {
                if self.all {
                    return;
                }
                let inner = self.line()[self.sel.clone()].to_string();
                let start = self.replace(self.sel.clone(), &format!("[{inner}]()"), false, cx);
                let caret = start + inner.len() + 3;
                self.sel = caret..caret;
                cx.notify();
            }
            Cmd::Wrap(l, r) => self.wrap(l, r, cx),
            Cmd::ClearFormat => {
                if self.all {
                    return;
                }
                let range = if self.sel.is_empty() {
                    block_prefix_len(self.line())..self.line().len()
                } else {
                    self.sel.clone()
                };
                let mut text = self.line()[range.clone()].to_string();
                for marker in ["**", "~~", "==", "%%", "`", "*", "$"] {
                    text = text.replace(marker, "");
                }
                self.replace(range, &text, false, cx);
            }
            Cmd::Prefix(p) => {
                let line = self.line().to_string();
                let indent = line.len() - line.trim_start().len();
                let keep_indent = if p.starts_with('#') || p.is_empty() {
                    0
                } else {
                    indent
                };
                let content = &line[block_prefix_len(&line)..];
                let new = format!("{}{p}{content}", &line[..keep_indent]);
                self.all = false;
                self.replace(0..line.len(), &new, false, cx);
            }
            Cmd::Insert(text, up) => {
                self.all = false;
                let len = self.line().len();
                if self.line().trim().is_empty() {
                    self.replace(0..len, text, false, cx);
                } else {
                    self.replace(len..len, &format!("\n{text}"), false, cx);
                }
                if up > 0 {
                    self.set_caret(self.row - up, usize::MAX, cx);
                }
            }
            Cmd::Cut => self.cut(&super::Cut, window, cx),
            Cmd::Copy => self.copy(&super::Copy, window, cx),
            Cmd::Paste => self.paste(&super::Paste, window, cx),
            Cmd::SelectAll => {
                self.all = true;
                cx.notify();
            }
            Cmd::RenameHeading => {
                let start = block_prefix_len(self.line());
                self.sel = start..self.line().len();
                self.reversed = false;
                self.all = false;
                cx.notify();
            }
        }
    }

    /// Wraps the selection in `l…r` (or inserts `lr` with the caret between) and keeps the
    /// wrapped text selected.
    pub(super) fn wrap(&mut self, l: &str, r: &str, cx: &mut Context<Self>) {
        if self.all {
            return;
        }
        let inner = self.line()[self.sel.clone()].to_string();
        let start = self.replace(self.sel.clone(), &format!("{l}{inner}{r}"), false, cx);
        let a = start + l.len();
        self.sel = a..a + inner.len();
        self.reversed = false;
        cx.notify();
    }

    fn menu_panel(
        &self,
        sections: Vec<Vec<Item>>,
        is_main: bool,
        cx: &mut Context<Self>,
    ) -> (AnyElement, Vec<(Sub, f32)>) {
        let mut tops = Vec::new();
        let mut y = PAD;
        let mut children = Vec::new();
        for (si, section) in sections.into_iter().enumerate() {
            if si > 0 {
                children.push(
                    div()
                        .h_px()
                        .my(px((SEP_H - 1.) / 2.))
                        .bg(c(0x3a3a3a))
                        .into_any_element(),
                );
                y += SEP_H;
            }
            for (ii, it) in section.into_iter().enumerate() {
                if let Some(sub) = it.sub {
                    tops.push((sub, y));
                }
                y += ITEM_H;
                children.push(self.menu_item(
                    SharedString::from(format!("menu-{is_main}-{si}-{ii}")),
                    it,
                    is_main,
                    cx,
                ));
            }
        }
        let panel = div()
            .occlude()
            .w(px(250.))
            .p(px(PAD))
            .flex()
            .flex_col()
            .bg(c(0x262626))
            .border_1()
            .border_color(c(0x3a3a3a))
            .rounded_lg()
            .shadow_lg()
            .font_family(UI)
            .children(children)
            .into_any_element();
        (panel, tops)
    }

    fn menu_item(
        &self,
        id: SharedString,
        it: Item,
        is_main: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (fg, icon_fg) = if it.enabled {
            (c(0xdcddde), c(0xb3b3b3))
        } else {
            (c(0x666666), c(0x555555))
        };
        let sub = it.sub;
        let cmd = it.cmd.filter(|_| it.enabled);
        div()
            .id(id)
            .h(px(ITEM_H))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .rounded(px(4.))
            .text_size(px(13.))
            .text_color(fg)
            .when(it.enabled, |d| d.hover(|s| s.bg(c(0x3a3a3a))))
            .child(
                svg()
                    .path(SharedString::from(format!("icons/notes/{}.svg", it.icon)))
                    .w(px(16.))
                    .h(px(16.))
                    .flex_none()
                    .text_color(icon_fg),
            )
            .child(div().flex_1().child(it.label))
            .when(!it.key.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(12.))
                        .text_color(c(0x888888))
                        .child(it.key),
                )
            })
            .when(sub.is_some(), |d| {
                d.child(
                    svg()
                        .path("icons/notes/chevron-right.svg")
                        .w(px(14.))
                        .h(px(14.))
                        .text_color(c(0x999999)),
                )
            })
            .when(is_main, |d| {
                d.on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    if *hovered && let Some(menu) = &mut this.menu {
                        if menu.sub != sub {
                            menu.sub = sub;
                            cx.notify();
                        }
                    }
                }))
            })
            .when_some(cmd, |d, cmd| {
                d.on_click(cx.listener(move |this, _, window, cx| this.run_cmd(cmd, window, cx)))
            })
            .into_any_element()
    }

    pub(super) fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu.as_ref()?;
        let (pos, open_sub) = (menu.pos, menu.sub);
        let has_sel = self.all || !self.sel.is_empty();
        let heading = matches!(markdown::classify(self.line()), Kind::Heading(..));
        let mut main = vec![
            vec![
                item("link", "Add link", "", Cmd::Link),
                item(
                    "external-link",
                    "Add external link",
                    "Ctrl+K",
                    Cmd::ExternalLink,
                ),
            ],
            vec![
                submenu("brush", "Format", Sub::Format),
                submenu("pilcrow", "Paragraph", Sub::Paragraph),
                submenu("row-insert-bottom", "Insert", Sub::Insert),
            ],
            vec![
                Item {
                    enabled: has_sel,
                    ..item("cut", "Cut", "Ctrl+X", Cmd::Cut)
                },
                Item {
                    enabled: has_sel,
                    ..item("copy", "Copy", "Ctrl+C", Cmd::Copy)
                },
                item("clipboard", "Paste", "Ctrl+V", Cmd::Paste),
                item("clipboard-text", "Paste as plain text", "", Cmd::Paste),
                item("select-all", "Select all", "Ctrl+A", Cmd::SelectAll),
            ],
        ];
        if heading {
            main.push(vec![item(
                "pencil",
                "Rename this heading…",
                "",
                Cmd::RenameHeading,
            )]);
        }
        let (main_panel, tops) = self.menu_panel(main, true, cx);
        let sub_panel = open_sub.map(|s| {
            let top = tops.iter().find(|(t, _)| *t == s).map_or(0., |(_, y)| *y) - PAD;
            let (panel, _) = self.menu_panel(sub_items(s), false, cx);
            div().mt(px(top)).ml(px(-2.)).child(panel)
        });
        Some(
            deferred(
                anchored()
                    .position(pos)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .child(main_panel)
                            .children(sub_panel),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}
