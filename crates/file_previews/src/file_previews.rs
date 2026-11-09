//! On-demand previews for CSV/TSV, spreadsheets, Jupyter notebooks and PDF text.
//! Nothing is read until a preview is opened; everything is dropped with the tab.

use std::{
    fs::File,
    io::BufReader,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use calamine::Reader as _;
use gpui::{
    Entity, EventEmitter, FocusHandle, Focusable, Image, ImageFormat, ImageSource, Task,
    UniformListScrollHandle, actions, img, uniform_list,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use serde_json::Value;
use ui::prelude::*;
use workspace::{Workspace, item::Item};

actions!(
    file_previews,
    [
        /// Opens a preview tab (table, notebook or PDF text) for the active file.
        OpenPreview
    ]
);

const MAX_ROWS: usize = 200_000;
const COLUMN_WIDTH: f32 = 160.;
const EXTENSIONS: &[&str] = &[
    "csv", "tsv", "xlsx", "xlsm", "xlsb", "xls", "ods", "ipynb", "pdf",
];

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenPreview, window, cx| {
            let path = workspace
                .active_item(cx)
                .and_then(|item| item.project_path(cx))
                .and_then(|path| workspace.project().read(cx).absolute_path(&path, cx));
            if let Some(path) = path {
                open_path(workspace, path, window, cx);
            }
        });
    })
    .detach();
}

/// Opens a preview tab for `path` in the active pane. Returns false if the extension is unsupported.
pub fn open_path(
    workspace: &mut Workspace,
    path: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    if !is_supported(&path) {
        return false;
    }
    let preview = FilePreview::new(path, cx);
    workspace.active_pane().clone().update(cx, |pane, cx| {
        pane.add_item(Box::new(preview), true, true, None, window, cx)
    });
    true
}

pub fn is_supported(path: &Path) -> bool {
    EXTENSIONS.contains(&extension(path).as_str())
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

struct Table {
    header: Vec<SharedString>,
    rows: Vec<Vec<SharedString>>,
    columns: usize,
    truncated: bool,
}

impl Table {
    fn new(mut rows: Vec<Vec<SharedString>>, truncated: bool) -> Self {
        let header = if rows.is_empty() {
            Vec::new()
        } else {
            rows.remove(0)
        };
        let columns = rows.iter().map(Vec::len).chain([header.len()]).max().unwrap_or(0);
        Self {
            header,
            rows,
            columns,
            truncated,
        }
    }
}

enum ParsedCell {
    Markdown(String),
    Code(String, Vec<ParsedOutput>),
}

enum ParsedOutput {
    Text(String),
    Png(Vec<u8>),
}

enum Loaded {
    Table(Vec<SharedString>, Table),
    Notebook(Vec<ParsedCell>),
    Pdf(Vec<SharedString>),
}

enum Cell {
    Markdown(Entity<Markdown>),
    Code(SharedString, Vec<Output>),
}

enum Output {
    Text(SharedString),
    Image(Arc<Image>),
}

enum Content {
    Loading,
    Error(SharedString),
    Table {
        sheets: Vec<SharedString>,
        active: usize,
        table: Option<Table>,
    },
    Notebook(Vec<Cell>),
    Pdf(Vec<SharedString>),
}

pub struct FilePreview {
    focus_handle: FocusHandle,
    path: PathBuf,
    title: SharedString,
    content: Content,
    list: UniformListScrollHandle,
    _load: Task<()>,
}

impl FilePreview {
    fn new(path: PathBuf, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            // Decoded notebook images live in the global asset cache; evict them with the tab.
            cx.on_release(|this: &mut Self, cx| this.release_images(cx))
                .detach();
            let title = path
                .file_name()
                .map(|name| format!("Preview {}", name.to_string_lossy()))
                .unwrap_or_else(|| "Preview".into())
                .into();
            let mut this = Self {
                focus_handle: cx.focus_handle(),
                path,
                title,
                content: Content::Loading,
                list: UniformListScrollHandle::new(),
                _load: Task::ready(()),
            };
            this.load(0, cx);
            this
        })
    }

    fn load(&mut self, sheet: usize, cx: &mut Context<Self>) {
        let path = self.path.clone();
        self._load = cx.spawn(async move |this, cx| {
            let loaded = cx.background_spawn(async move { load(&path, sheet) }).await;
            this.update(cx, |this, cx| {
                this.content = match loaded {
                    Ok(Loaded::Table(sheets, table)) => Content::Table {
                        sheets,
                        active: sheet,
                        table: Some(table),
                    },
                    Ok(Loaded::Pdf(pages)) => Content::Pdf(pages),
                    Ok(Loaded::Notebook(cells)) => Content::Notebook(
                        cells
                            .into_iter()
                            .map(|cell| match cell {
                                ParsedCell::Markdown(source) => Cell::Markdown(
                                    cx.new(|cx| Markdown::new(source.into(), None, None, cx)),
                                ),
                                ParsedCell::Code(source, outputs) => Cell::Code(
                                    source.into(),
                                    outputs
                                        .into_iter()
                                        .map(|output| match output {
                                            ParsedOutput::Text(text) => Output::Text(text.into()),
                                            ParsedOutput::Png(bytes) => Output::Image(Arc::new(
                                                Image::from_bytes(ImageFormat::Png, bytes),
                                            )),
                                        })
                                        .collect(),
                                ),
                            })
                            .collect(),
                    ),
                    Err(error) => Content::Error(format!("Preview failed: {error:#}").into()),
                };
                cx.notify();
            })
            .ok();
        });
    }

    fn select_sheet(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Content::Table { active, table, .. } = &mut self.content {
            // Drop the previous sheet before parsing the next one, so only one is in memory.
            *active = ix;
            *table = None;
            self.list = UniformListScrollHandle::new();
            self.load(ix, cx);
            cx.notify();
        }
    }

    fn release_images(&mut self, cx: &mut App) {
        if let Content::Notebook(cells) = &self.content {
            for cell in cells {
                if let Cell::Code(_, outputs) = cell {
                    for output in outputs {
                        if let Output::Image(image) = output {
                            ImageSource::Image(image.clone()).remove_asset(cx);
                        }
                    }
                }
            }
        }
    }

    fn render_table(
        &self,
        sheets: &[SharedString],
        active: usize,
        table: Option<&Table>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tabs = (!sheets.is_empty()).then(|| {
            h_flex()
                .gap_1()
                .p_1()
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .children(sheets.iter().enumerate().map(|(ix, name)| {
                    Button::new(("sheet", ix), name.clone())
                        .toggle_state(ix == active)
                        .on_click(cx.listener(move |this, _, _, cx| this.select_sheet(ix, cx)))
                }))
        });
        let Some(table) = table else {
            return v_flex()
                .size_full()
                .children(tabs)
                .child(Label::new("Loading…").color(Color::Muted))
                .into_any_element();
        };
        let border = cx.theme().colors().border_variant;
        let header_bg = cx.theme().colors().title_bar_background;
        let rows = uniform_list(
            "file-preview-rows",
            table.rows.len(),
            cx.processor(|this, range: Range<usize>, _, cx| {
                let Content::Table {
                    table: Some(table), ..
                } = &this.content
                else {
                    return Vec::new();
                };
                let border = cx.theme().colors().border_variant;
                range
                    .map(|ix| render_row(&table.rows[ix], border))
                    .collect()
            }),
        )
        .track_scroll(&self.list)
        .flex_1();

        v_flex()
            .size_full()
            .children(tabs)
            .when(table.truncated, |this| {
                this.child(
                    Label::new(format!("Showing the first {MAX_ROWS} rows only."))
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .child(
                // Horizontal scroll wraps both header and rows; the header stays put vertically.
                div().id("file-preview-table").flex_1().min_h_0().overflow_x_scroll().child(
                    v_flex()
                        .h_full()
                        .w(px(COLUMN_WIDTH * table.columns.max(1) as f32))
                        .child(render_row(&table.header, border).bg(header_bg))
                        .child(rows),
                ),
            )
            .into_any_element()
    }
}

fn render_row(cells: &[SharedString], border: gpui::Hsla) -> Div {
    h_flex()
        .border_b_1()
        .border_color(border)
        .children(cells.iter().map(|cell| {
            div()
                .w(px(COLUMN_WIDTH))
                .flex_none()
                .px_2()
                .overflow_hidden()
                .border_r_1()
                .border_color(border)
                .child(Label::new(cell.clone()).single_line().truncate())
        }))
}

fn load(path: &Path, sheet: usize) -> Result<Loaded> {
    match extension(path).as_str() {
        "csv" => load_csv(path, b','),
        "tsv" => load_csv(path, b'\t'),
        "ipynb" => load_notebook(path),
        "pdf" => {
            let pages = pdf_extract::extract_text_from_mem_by_pages(&std::fs::read(path)?)
                .map_err(|error| anyhow!("{error}"))?;
            Ok(Loaded::Pdf(pages.into_iter().map(Into::into).collect()))
        }
        _ => load_sheet(path, sheet),
    }
}

fn load_csv(path: &Path, delimiter: u8) -> Result<Loaded> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_path(path)?;
    let mut rows = Vec::new();
    let mut truncated = false;
    for record in reader.byte_records() {
        if rows.len() > MAX_ROWS {
            truncated = true;
            break;
        }
        rows.push(
            record?
                .iter()
                .map(|field| String::from_utf8_lossy(field).into_owned().into())
                .collect(),
        );
    }
    Ok(Loaded::Table(Vec::new(), Table::new(rows, truncated)))
}

fn load_sheet(path: &Path, sheet: usize) -> Result<Loaded> {
    let mut workbook = calamine::open_workbook_auto(path)?;
    let names = workbook.sheet_names();
    let name = names.get(sheet).context("workbook has no sheets")?;
    let range = workbook.worksheet_range(name)?;
    let rows = range
        .rows()
        .take(MAX_ROWS + 1)
        .map(|row| row.iter().map(|cell| cell.to_string().into()).collect())
        .collect();
    let table = Table::new(rows, range.height() > MAX_ROWS + 1);
    Ok(Loaded::Table(names.into_iter().map(Into::into).collect(), table))
}

/// Notebook strings are either a string or an array of lines.
fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(lines) => lines.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

fn load_notebook(path: &Path) -> Result<Loaded> {
    let notebook: Value = serde_json::from_reader(BufReader::new(File::open(path)?))?;
    let cells = notebook["cells"]
        .as_array()
        .context("not a Jupyter notebook")?
        .iter()
        .map(|cell| {
            let source = text(&cell["source"]);
            if cell["cell_type"] == "markdown" {
                return ParsedCell::Markdown(source);
            }
            let outputs = cell["outputs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|output| {
                    let data = &output["data"];
                    if let Some(png) = data["image/png"].as_str() {
                        let png: String = png.chars().filter(|c| !c.is_whitespace()).collect();
                        return base64::engine::general_purpose::STANDARD
                            .decode(png)
                            .ok()
                            .map(ParsedOutput::Png);
                    }
                    let text = match output["output_type"].as_str() {
                        Some("stream") => text(&output["text"]),
                        Some("error") => format!(
                            "{}: {}",
                            text(&output["ename"]),
                            text(&output["evalue"])
                        ),
                        _ => text(&data["text/plain"]),
                    };
                    (!text.is_empty()).then_some(ParsedOutput::Text(text))
                })
                .collect();
            ParsedCell::Code(source, outputs)
        })
        .collect();
    Ok(Loaded::Notebook(cells))
}

impl EventEmitter<()> for FilePreview {}

impl Focusable for FilePreview {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for FilePreview {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileDoc))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.path.to_string_lossy().into_owned().into())
    }
}

impl Render for FilePreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let (code_bg, editor_bg) = (colors.surface_background, colors.editor_background);
        let body = match &self.content {
            Content::Loading => Label::new("Loading…")
                .color(Color::Muted)
                .into_any_element(),
            Content::Error(error) => Label::new(error.clone())
                .color(Color::Error)
                .into_any_element(),
            Content::Table {
                sheets,
                active,
                table,
            } => self.render_table(sheets, *active, table.as_ref(), cx),
            Content::Notebook(cells) => scroll_column("file-preview-notebook")
                .children(cells.iter().map(|cell| match cell {
                    Cell::Markdown(markdown) => MarkdownElement::new(
                        markdown.clone(),
                        MarkdownStyle::themed(MarkdownFont::Preview, window, cx),
                    )
                    .into_any_element(),
                    Cell::Code(source, outputs) => v_flex()
                        .gap_1()
                        .child(
                            div()
                                .p_2()
                                .rounded_sm()
                                .bg(code_bg)
                                .font_buffer(cx)
                                .text_sm()
                                .child(source.clone()),
                        )
                        .children(outputs.iter().map(|output| match output {
                            Output::Text(text) => div()
                                .px_2()
                                .font_buffer(cx)
                                .text_sm()
                                .text_color(Color::Muted.color(cx))
                                .child(text.clone())
                                .into_any_element(),
                            Output::Image(image) => {
                                img(image.clone()).max_w_full().into_any_element()
                            }
                        }))
                        .into_any_element(),
                }))
                .into_any_element(),
            Content::Pdf(pages) => scroll_column("file-preview-pdf")
                .child(
                    Label::new("Text preview: layout, images and fonts are not rendered.")
                        .color(Color::Muted),
                )
                .children(pages.iter().enumerate().map(|(ix, page)| {
                    v_flex()
                        .gap_1()
                        .child(
                            Label::new(format!("Page {}", ix + 1))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(div().child(page.clone()))
                }))
                .into_any_element(),
        };
        v_flex()
            .key_context("FilePreview")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(editor_bg)
            .child(body)
    }
}

// ponytail: notebook and PDF pages render every cell each frame; switch to `list` if huge files lag.
fn scroll_column(id: &'static str) -> gpui::Stateful<Div> {
    v_flex()
        .id(id)
        .size_full()
        .overflow_y_scroll()
        .p_4()
        .gap_3()
}
