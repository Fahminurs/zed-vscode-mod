//! A VS Code style "Search" panel that lives in a dock (left sidebar by default).
//!
//! Type in the query box and results are listed per file; click a line to open
//! the file at that match. Supports case / whole word / regex toggles and
//! include / exclude glob filters, reusing the regular project search engine.

use std::{ops::Range, path::Path, pin::pin, time::Duration};

use collections::HashSet;
use editor::{Editor, EditorEvent};
use file_icons::FileIcons;
use futures::StreamExt as _;
use gpui::{
    Action, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Pixels, Render, SharedString, Subscription, Task, WeakEntity, Window, actions, px,
    uniform_list,
};
use project::{
    Project, ProjectPath,
    search::{SearchQuery, SearchResult},
};
use ui::{HighlightedLabel, IconButtonShape, ListItem, Tooltip, prelude::*};
use util::{ResultExt as _, paths::PathMatcher};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{SearchOption, SearchOptions, project_search::split_glob_patterns};

actions!(
    search_panel,
    [
        /// Toggles focus on the search panel in the sidebar.
        ToggleFocus,
    ]
);

const SEARCH_PANEL_KEY: &str = "SearchPanel";
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
const MAX_MATCHES: usize = 10_000;
const PREVIEW_CONTEXT: usize = 40;
const PREVIEW_MAX: usize = 160;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<SearchPanel>(window, cx);
        });
    })
    .detach();
}

struct MatchLine {
    row: u32,
    column: u32,
    preview: SharedString,
    highlights: Vec<usize>,
    /// Byte range of the match inside `preview`.
    preview_hit: Range<usize>,
    range: Range<language::Anchor>,
    /// Full line(s) containing the match, and the match inside it (for replacement).
    line: String,
    hit: Range<usize>,
}

struct FileResult {
    path: ProjectPath,
    buffer: Entity<language::Buffer>,
    name: SharedString,
    dir: SharedString,
    matches: Vec<MatchLine>,
}

#[derive(Clone)]
enum Entry {
    Dir(SharedString),
    File(usize),
    Match(usize, usize),
}

enum Status {
    Idle,
    Searching,
    Done { limit_reached: bool },
    Error(SharedString),
}

pub struct SearchPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    query_editor: Entity<Editor>,
    include_editor: Entity<Editor>,
    exclude_editor: Entity<Editor>,
    replace_editor: Entity<Editor>,
    options: SearchOptions,
    query: Option<SearchQuery>,
    show_filters: bool,
    show_replace: bool,
    preserve_case: bool,
    tree_view: bool,
    collapsed_dirs: HashSet<SharedString>,
    position: DockPosition,
    files: Vec<FileResult>,
    collapsed: HashSet<usize>,
    entries: Vec<Entry>,
    match_count: usize,
    status: Status,
    pending_search: Option<Task<()>>,
    search_handle: Option<Task<()>>,
    debounce: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SearchPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            Self::new(workspace, window, cx)
        })
    }

    fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let workspace_handle = cx.entity().downgrade();
        cx.new(|cx| {
            let make_editor = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
                let placeholder = placeholder.to_string();
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text(&placeholder, window, cx);
                    editor
                })
            };
            let query_editor = make_editor("Search", window, cx);
            let include_editor = make_editor("files to include", window, cx);
            let exclude_editor = make_editor("files to exclude", window, cx);
            let replace_editor = make_editor("Replace", window, cx);

            let mut subscriptions = Vec::new();
            for editor in [&query_editor, &include_editor, &exclude_editor] {
                subscriptions.push(cx.subscribe_in(
                    editor,
                    window,
                    |this: &mut Self, _, event: &EditorEvent, _window, cx| {
                        if let EditorEvent::BufferEdited = event {
                            this.schedule_search(cx);
                        }
                    },
                ));
            }
            subscriptions.push(cx.subscribe_in(
                &replace_editor,
                window,
                |_: &mut Self, _, event: &EditorEvent, _window, cx| {
                    if let EditorEvent::BufferEdited = event {
                        cx.notify();
                    }
                },
            ));
            subscriptions.push(cx.observe_global::<FileIcons>(|_, cx| cx.notify()));

            Self {
                workspace: workspace_handle,
                project,
                focus_handle: cx.focus_handle(),
                query_editor,
                include_editor,
                exclude_editor,
                replace_editor,
                options: SearchOptions::NONE,
                query: None,
                show_filters: false,
                show_replace: true,
                preserve_case: false,
                tree_view: false,
                collapsed_dirs: HashSet::default(),
                position: DockPosition::Left,
                files: Vec::new(),
                collapsed: HashSet::default(),
                entries: Vec::new(),
                match_count: 0,
                status: Status::Idle,
                pending_search: None,
                search_handle: None,
                debounce: None,
                _subscriptions: subscriptions,
            }
        })
    }

    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            this.update(cx, |this, cx| this.search(cx)).ok();
        }));
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        self.debounce = None;
        self.search(cx);
    }

    fn toggle_option(&mut self, option: SearchOptions, cx: &mut Context<Self>) {
        self.options.toggle(option);
        self.search(cx);
    }

    fn parse_filter(&self, editor: &Entity<Editor>, cx: &App) -> anyhow::Result<PathMatcher> {
        let text = editor.read(cx).text(cx);
        let path_style = self.project.read(cx).path_style(cx);
        let patterns = split_glob_patterns(&text)
            .into_iter()
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        Ok(PathMatcher::new(&patterns, path_style)?)
    }

    fn clear_results(&mut self) {
        self.pending_search = None;
        self.search_handle = None;
        self.query = None;
        self.files.clear();
        self.collapsed.clear();
        self.collapsed_dirs.clear();
        self.entries.clear();
        self.match_count = 0;
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        let text = self.query_editor.read(cx).text(cx);
        self.clear_results();
        if text.is_empty() {
            self.status = Status::Idle;
            cx.notify();
            return;
        }

        let filters = if self.show_filters {
            self.parse_filter(&self.include_editor, cx)
                .and_then(|include| Ok((include, self.parse_filter(&self.exclude_editor, cx)?)))
        } else {
            Ok((PathMatcher::default(), PathMatcher::default()))
        };
        let (include, exclude) = match filters {
            Ok(filters) => filters,
            Err(error) => {
                self.status = Status::Error(error.to_string().into());
                cx.notify();
                return;
            }
        };
        let match_full_paths = self.project.read(cx).visible_worktrees(cx).count() > 1;
        let query = match self
            .options
            .build_query(text, include, exclude, match_full_paths, None)
        {
            Ok(query) => query,
            Err(error) => {
                self.status = Status::Error(error.to_string().into());
                cx.notify();
                return;
            }
        };

        self.query = Some(query.clone());
        let results = self
            .project
            .update(cx, |project, cx| project.search(query, cx));
        self.search_handle = Some(results.task_handle);
        let rx = results.rx;
        self.status = Status::Searching;
        cx.notify();

        self.pending_search = Some(cx.spawn(async move |this, cx| {
            let mut chunks = pin!(rx.ready_chunks(256));
            let mut limit_reached = false;
            while let Some(chunk) = chunks.next().await {
                let mut new_files = Vec::new();
                for result in chunk {
                    match result {
                        SearchResult::Buffer { buffer, ranges } => {
                            if let Some(file) = buffer
                                .read_with(cx, |b, cx| file_result(buffer.clone(), b, &ranges, cx))
                            {
                                new_files.push(file);
                            }
                        }
                        SearchResult::LimitReached => limit_reached = true,
                        SearchResult::WaitingForScan | SearchResult::Searching => {}
                    }
                }
                let keep_going = this
                    .update(cx, |this, cx| {
                        for file in new_files {
                            this.match_count += file.matches.len();
                            this.files.push(file);
                        }
                        this.rebuild_entries();
                        cx.notify();
                        this.match_count < MAX_MATCHES
                    })
                    .unwrap_or(false);
                if !keep_going {
                    limit_reached = true;
                    break;
                }
            }
            this.update(cx, |this, cx| {
                this.status = Status::Done { limit_reached };
                this.search_handle = None;
                cx.notify();
            })
            .ok();
        }));
    }

    fn rebuild_entries(&mut self) {
        let mut entries = Vec::new();
        let push_file = |entries: &mut Vec<Entry>, file_ix: usize| {
            entries.push(Entry::File(file_ix));
            if !self.collapsed.contains(&file_ix) {
                entries.extend(
                    (0..self.files[file_ix].matches.len()).map(|ix| Entry::Match(file_ix, ix)),
                );
            }
        };
        if self.tree_view {
            // ponytail: O(dirs * files) grouping, fine under MAX_MATCHES; index by dir if it gets slow.
            let mut dirs: Vec<&SharedString> = Vec::new();
            for file in &self.files {
                if !dirs.contains(&&file.dir) {
                    dirs.push(&file.dir);
                }
            }
            for dir in dirs {
                entries.push(Entry::Dir(dir.clone()));
                if !self.collapsed_dirs.contains(dir) {
                    for (file_ix, _) in self.files.iter().enumerate().filter(|(_, f)| &f.dir == dir)
                    {
                        push_file(&mut entries, file_ix);
                    }
                }
            }
        } else {
            for file_ix in 0..self.files.len() {
                push_file(&mut entries, file_ix);
            }
        }
        self.entries = entries;
    }

    fn toggle_dir_collapsed(&mut self, dir: SharedString, cx: &mut Context<Self>) {
        if !self.collapsed_dirs.remove(&dir) {
            self.collapsed_dirs.insert(dir);
        }
        self.rebuild_entries();
        cx.notify();
    }

    fn replace_query(&self, cx: &App) -> Option<SearchQuery> {
        Some(
            self.query
                .clone()?
                .with_replacement(self.replace_editor.read(cx).text(cx)),
        )
    }

    /// Query used to render replacement previews; `None` when there is nothing to preview.
    fn preview_query(&self, cx: &App) -> Option<SearchQuery> {
        if !self.show_replace || self.replace_editor.read(cx).text(cx).is_empty() {
            return None;
        }
        self.replace_query(cx)
    }

    fn replacement(&self, query: &SearchQuery, line: &MatchLine) -> Option<String> {
        let new = query.replacement_for(&line.line, line.hit.clone())?;
        Some(if self.preserve_case {
            preserve_case(&line.line[line.hit.clone()], &new)
        } else {
            new.into_owned()
        })
    }

    fn replace_all(&mut self, cx: &mut Context<Self>) {
        let Some(query) = self.replace_query(cx) else {
            return;
        };
        for file in &self.files {
            let edits = file
                .matches
                .iter()
                .filter_map(|line| Some((line.range.clone(), self.replacement(&query, line)?)))
                .collect::<Vec<_>>();
            file.buffer.update(cx, |buffer, cx| {
                buffer.edit(edits, None, cx);
            });
        }
        self.search(cx);
    }

    fn toggle_collapsed(&mut self, file_ix: usize, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&file_ix) {
            self.collapsed.insert(file_ix);
        }
        self.rebuild_entries();
        cx.notify();
    }

    fn set_all_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        self.collapsed = if collapsed {
            (0..self.files.len()).collect()
        } else {
            HashSet::default()
        };
        self.rebuild_entries();
        cx.notify();
    }

    fn open_match(
        &mut self,
        file_ix: usize,
        match_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self.files.get(file_ix) else {
            return;
        };
        let Some(line) = file.matches.get(match_ix) else {
            return;
        };
        let (path, row, column) = (file.path.clone(), line.row, line.column);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let open_task = workspace.update(cx, |workspace, cx| {
            workspace.open_path_preview(path, None, true, true, true, window, cx)
        });
        cx.spawn_in(window, async move |_, cx| {
            let item = open_task.await.log_err()?;
            if let Some(editor) = item.downcast::<Editor>() {
                editor
                    .downgrade()
                    .update_in(cx, |editor, window, cx| {
                        editor.go_to_singleton_buffer_point(
                            text::Point::new(row, column),
                            window,
                            cx,
                        );
                    })
                    .log_err();
            }
            Some(())
        })
        .detach();
    }

    fn render_input(&self, editor: &Entity<Editor>, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_1()
            .min_w_0()
            .h_7()
            .px_2()
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_sm()
            .bg(cx.theme().colors().editor_background)
            .child(editor.clone())
    }

    fn render_option(
        &self,
        option: SearchOption,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let flag = option.as_options();
        let label = option.label();
        IconButton::new(SharedString::from(label), option.icon())
            .shape(IconButtonShape::Square)
            .icon_size(IconSize::Small)
            .toggle_state(self.options.contains(flag))
            .tooltip(Tooltip::text(label))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.toggle_option(flag, cx);
            }))
    }

    fn render_status(&self, cx: &Context<Self>) -> impl IntoElement {
        let text: SharedString = match &self.status {
            Status::Idle => "Type to search across all files".into(),
            Status::Searching if self.match_count == 0 => "Searching…".into(),
            Status::Searching => format!(
                "Searching… {} results in {} files",
                self.match_count,
                self.files.len()
            )
            .into(),
            Status::Done { .. } if self.match_count == 0 => "No results found".into(),
            Status::Done { limit_reached } => format!(
                "{} result{} in {} file{}{}",
                self.match_count,
                if self.match_count == 1 { "" } else { "s" },
                self.files.len(),
                if self.files.len() == 1 { "" } else { "s" },
                if *limit_reached {
                    " (limit reached)"
                } else {
                    ""
                }
            )
            .into(),
            Status::Error(error) => error.clone(),
        };
        let color = if matches!(self.status, Status::Error(_)) {
            Color::Error
        } else {
            Color::Muted
        };
        h_flex()
            .px_2()
            .pb_1()
            .w_full()
            .justify_between()
            .child(Label::new(text).size(LabelSize::Small).color(color))
    }

    fn render_entry(
        &self,
        ix: usize,
        replace: Option<&SearchQuery>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let depth = self.tree_view as usize;
        match self.entries[ix].clone() {
            Entry::Dir(dir) => {
                let collapsed = self.collapsed_dirs.contains(&dir);
                ListItem::new(("search-dir", ix))
                    .start_slot(
                        h_flex()
                            .gap_1()
                            .child(
                                Icon::new(if collapsed {
                                    IconName::ChevronRight
                                } else {
                                    IconName::ChevronDown
                                })
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                            )
                            .child(Icon::new(IconName::Folder).size(IconSize::Small)),
                    )
                    .child(Label::new(dir.clone()).single_line().truncate())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.toggle_dir_collapsed(dir.clone(), cx);
                    }))
                    .into_any_element()
            }
            Entry::File(file_ix) => {
                let file = &self.files[file_ix];
                let collapsed = self.collapsed.contains(&file_ix);
                let icon = FileIcons::get_icon(Path::new(file.name.as_ref()), cx)
                    .map(|path| {
                        Icon::from_path(path)
                            .size(IconSize::Small)
                            .into_any_element()
                    })
                    .unwrap_or_else(|| {
                        Icon::new(IconName::File)
                            .size(IconSize::Small)
                            .into_any_element()
                    });
                ListItem::new(("search-file", ix))
                    .indent_level(depth)
                    .indent_step_size(px(20.))
                    .start_slot(
                        h_flex()
                            .gap_1()
                            .child(
                                Icon::new(if collapsed {
                                    IconName::ChevronRight
                                } else {
                                    IconName::ChevronDown
                                })
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                            )
                            .child(icon),
                    )
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_1p5()
                            .child(Label::new(file.name.clone()).single_line())
                            .when(!self.tree_view, |this| {
                                this.child(
                                    Label::new(file.dir.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .single_line()
                                        .truncate(),
                                )
                            }),
                    )
                    .end_slot(
                        div()
                            .px_1p5()
                            .rounded_full()
                            .bg(cx.theme().colors().element_background)
                            .child(
                                Label::new(file.matches.len().to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.toggle_collapsed(file_ix, cx);
                    }))
                    .into_any_element()
            }
            Entry::Match(file_ix, match_ix) => {
                let line = &self.files[file_ix].matches[match_ix];
                let label = match replace.and_then(|query| self.replacement(query, line)) {
                    Some(new) => {
                        let hit = line.preview_hit.clone();
                        let part = |text: &str| {
                            Label::new(SharedString::from(text.to_string()))
                                .size(LabelSize::Small)
                                .single_line()
                        };
                        h_flex()
                            .min_w_0()
                            .overflow_hidden()
                            .child(part(&line.preview[..hit.start]))
                            .child(
                                part(&line.preview[hit.clone()])
                                    .color(Color::Deleted)
                                    .strikethrough(),
                            )
                            .child(part(&new.replace('\n', "⏎")).color(Color::Created))
                            .child(part(&line.preview[hit.end..]).truncate())
                            .into_any_element()
                    }
                    None => HighlightedLabel::new(line.preview.clone(), line.highlights.clone())
                        .size(LabelSize::Small)
                        .single_line()
                        .truncate()
                        .into_any_element(),
                };
                ListItem::new(("search-match", ix))
                    .indent_level(depth + 1)
                    .indent_step_size(px(20.))
                    .child(label)
                    .tooltip(Tooltip::text(format!("Line {}", line.row + 1)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.open_match(file_ix, match_ix, window, cx);
                    }))
                    .into_any_element()
            }
        }
    }
}

/// Collect per-line previews for every match in one buffer.
fn file_result(
    buffer_entity: Entity<language::Buffer>,
    buffer: &language::Buffer,
    ranges: &[Range<language::Anchor>],
    cx: &App,
) -> Option<FileResult> {
    if ranges.is_empty() {
        return None;
    }
    let file = buffer.file()?;
    let path = ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    };
    let full_path = file.full_path(cx);
    let name: SharedString = full_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
        .into();
    let dir: SharedString = full_path
        .parent()
        .map(|dir| dir.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
        .into();

    let mut matches = Vec::new();
    let mut last_row = None;
    for range in ranges {
        let start: usize = buffer.summary_for_anchor(&range.start);
        let end: usize = buffer.summary_for_anchor(&range.end);
        let point = buffer.offset_to_point(start);
        // VS Code shows one entry per match; keep that but skip exact duplicates.
        if last_row == Some((point.row, point.column)) {
            continue;
        }
        last_row = Some((point.row, point.column));

        let line_start = buffer.point_to_offset(text::Point::new(point.row, 0));
        let line_end =
            buffer.point_to_offset(text::Point::new(point.row, buffer.line_len(point.row)));
        let line: String = buffer.text_for_range(line_start..line_end).collect();
        let (preview, preview_hit) =
            make_preview(&line, start - line_start, end.min(line_end) - line_start);
        let highlights = preview[preview_hit.clone()]
            .char_indices()
            .map(|(ix, _)| preview_hit.start + ix)
            .collect();
        let end_row = buffer.offset_to_point(end).row;
        let full_end = buffer.point_to_offset(text::Point::new(end_row, buffer.line_len(end_row)));
        matches.push(MatchLine {
            row: point.row,
            column: point.column,
            preview: preview.into(),
            highlights,
            preview_hit,
            range: range.clone(),
            line: buffer.text_for_range(line_start..full_end).collect(),
            hit: start - line_start..end - line_start,
        });
    }
    Some(FileResult {
        path,
        buffer: buffer_entity,
        name,
        dir,
        matches,
    })
}

/// Trim a line around the match and return the match's byte range in the preview.
fn make_preview(line: &str, match_start: usize, match_end: usize) -> (String, Range<usize>) {
    let match_start = floor_char_boundary(line, match_start.min(line.len()));
    let match_end = floor_char_boundary(line, match_end.max(match_start).min(line.len()));

    let leading_ws = line.len() - line.trim_start().len();
    let mut from = leading_ws.min(match_start);
    if match_start - from > PREVIEW_CONTEXT {
        from = floor_char_boundary(line, match_start - PREVIEW_CONTEXT);
    }
    let to = floor_char_boundary(line, (from + PREVIEW_MAX).max(match_end).min(line.len()));

    let prefix = if from > leading_ws { "…" } else { "" };
    let mut preview = String::with_capacity(to - from + 3);
    preview.push_str(prefix);
    preview.push_str(&line[from..to]);
    let start = match_start - from + prefix.len();
    (preview, start..start + match_end - match_start)
}

/// VS Code "Preserve Case": ALL CAPS -> uppercase, Capitalized -> capitalize, else as typed.
fn preserve_case(old: &str, new: &str) -> String {
    if old.chars().any(char::is_alphabetic) && !old.chars().any(char::is_lowercase) {
        new.to_uppercase()
    } else if old.chars().next().is_some_and(char::is_uppercase) {
        let mut chars = new.chars();
        chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
            .unwrap_or_default()
    } else {
        new.to_string()
    }
}

#[cfg(test)]
#[test]
fn test_preserve_case() {
    assert_eq!(preserve_case("FOO", "bar"), "BAR");
    assert_eq!(preserve_case("Foo", "bar"), "Bar");
    assert_eq!(preserve_case("foo", "bAr"), "bAr");
}

fn floor_char_boundary(text: &str, mut ix: usize) -> usize {
    while ix > 0 && !text.is_char_boundary(ix) {
        ix -= 1;
    }
    ix
}

impl Render for SearchPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = window;
        let entries_len = self.entries.len();
        let has_results = !self.files.is_empty();
        let all_collapsed = has_results && self.collapsed.len() == self.files.len();
        let header_button = |id: &'static str, icon: IconName, tooltip: &'static str| {
            IconButton::new(id, icon)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text(tooltip))
        };
        v_flex()
            .id("search-panel")
            .key_context("SearchPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .size_full()
            .overflow_hidden()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .justify_between()
                    .child(
                        Label::new("SEARCH")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                header_button("refresh", IconName::RotateCw, "Refresh").on_click(
                                    cx.listener(|this, _: &ClickEvent, _, cx| this.search(cx)),
                                ),
                            )
                            .child(
                                header_button("clear", IconName::ListX, "Clear Search Results")
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.query_editor.update(cx, |editor, cx| {
                                            editor.set_text("", window, cx)
                                        });
                                        this.debounce = None;
                                        this.search(cx);
                                    })),
                            )
                            .child(
                                header_button("tree-view", IconName::ListTree, "View as Tree")
                                    .toggle_state(self.tree_view)
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.tree_view = !this.tree_view;
                                        this.rebuild_entries();
                                        cx.notify();
                                    })),
                            )
                            .child(
                                header_button(
                                    "collapse-all",
                                    if all_collapsed {
                                        IconName::ExpandVertical
                                    } else {
                                        IconName::ListCollapse
                                    },
                                    if all_collapsed {
                                        "Expand All"
                                    } else {
                                        "Collapse All"
                                    },
                                )
                                .disabled(!has_results)
                                .on_click(cx.listener(
                                    move |this, _: &ClickEvent, _, cx| {
                                        this.set_all_collapsed(!all_collapsed, cx);
                                    },
                                )),
                            )
                            .child(
                                header_button(
                                    "toggle-filters",
                                    IconName::Filter,
                                    "Toggle Search Details",
                                )
                                .toggle_state(self.show_filters)
                                .on_click(cx.listener(
                                    |this, _: &ClickEvent, _, cx| {
                                        this.show_filters = !this.show_filters;
                                        this.search(cx);
                                    },
                                )),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .px_2()
                    .gap_1()
                    .items_start()
                    .child(
                        IconButton::new(
                            "toggle-replace",
                            if self.show_replace {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            },
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Toggle Replace"))
                        .on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.show_replace = !this.show_replace;
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(self.render_input(&self.query_editor, cx))
                                    .child(self.render_option(SearchOption::CaseSensitive, cx))
                                    .child(self.render_option(SearchOption::WholeWord, cx))
                                    .child(self.render_option(SearchOption::Regex, cx)),
                            )
                            .when(self.show_replace, |this| {
                                this.child(
                                    h_flex()
                                        .gap_1()
                                        .child(self.render_input(&self.replace_editor, cx))
                                        .child(
                                            Button::new("preserve-case", "AB")
                                                .label_size(LabelSize::XSmall)
                                                .toggle_state(self.preserve_case)
                                                .tooltip(Tooltip::text("Preserve Case"))
                                                .on_click(cx.listener(
                                                    |this, _: &ClickEvent, _, cx| {
                                                        this.preserve_case = !this.preserve_case;
                                                        cx.notify();
                                                    },
                                                )),
                                        )
                                        .child(
                                            IconButton::new("replace-all", IconName::ReplaceAll)
                                                .shape(IconButtonShape::Square)
                                                .icon_size(IconSize::Small)
                                                .disabled(!has_results)
                                                .tooltip(Tooltip::text("Replace All"))
                                                .on_click(cx.listener(
                                                    |this, _: &ClickEvent, _, cx| {
                                                        this.replace_all(cx)
                                                    },
                                                )),
                                        ),
                                )
                            }),
                    ),
            )
            .when(self.show_filters, |this| {
                this.child(
                    v_flex()
                        .px_2()
                        .pt_1()
                        .gap_1()
                        .child(self.render_input(&self.include_editor, cx))
                        .child(self.render_input(&self.exclude_editor, cx)),
                )
            })
            .child(div().pt_1().child(self.render_status(cx)))
            .child(
                uniform_list(
                    "search-panel-entries",
                    entries_len,
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        let replace = this.preview_query(cx);
                        range
                            .map(|ix| this.render_entry(ix, replace.as_ref(), cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .size_full(),
            )
    }
}

impl Focusable for SearchPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for SearchPanel {}

impl Panel for SearchPanel {
    fn persistent_name() -> &'static str {
        "Search Panel"
    }

    fn panel_key() -> &'static str {
        SEARCH_PANEL_KEY
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.query_editor.focus_handle(cx)
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::MagnifyingGlass)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Search")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        2
    }
}
