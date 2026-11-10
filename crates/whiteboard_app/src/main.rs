#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Standalone whiteboard: the `whiteboard` crate in its own window, without Zed.

use std::path::PathBuf;

use gpui::{
    App, Bounds, Context, Entity, Focusable, KeyBinding, PathPromptOptions, PromptLevel,
    Subscription, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div, prelude::*,
    px, size,
};
use gpui_platform::application;
use whiteboard::{ItemEvent, Whiteboard};

actions!(whiteboard_app, [New, Open, Save, SaveAs, Quit]);

struct Root {
    board: Entity<Whiteboard>,
    _sub: Subscription,
}

impl Root {
    fn new(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let board = cx.new(Whiteboard::new_standalone);
        let _sub = cx.subscribe_in(&board, window, |this, _, _: &ItemEvent, window, cx| {
            this.update_title(window, cx)
        });
        board.focus_handle(cx).focus(window, cx);
        let mut this = Self { board, _sub };
        this.load(path, window, cx);
        this
    }

    fn update_title(&self, window: &mut Window, cx: &App) {
        let board = self.board.read(cx);
        let name = board
            .path()
            .and_then(|p| p.file_name())
            .map_or("Untitled".into(), |n| n.to_string_lossy());
        let dirty = if board.is_dirty() { " •" } else { "" };
        window.set_window_title(&format!("{name}{dirty} - Whiteboard"));
    }

    fn load(&mut self, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = self.board.update(cx, |b, cx| b.open(path, cx)) {
            show_error(e, window, cx);
        }
    }

    fn write(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = self.board.update(cx, |b, cx| b.save_to(path, window, cx)) {
            show_error(e, window, cx);
        }
    }

    fn save(&mut self, save_as: bool, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.board.read(cx).path().cloned();
        if let Some(path) = current.clone().filter(|_| !save_as) {
            return self.write(path, window, cx);
        }
        let dir = current
            .as_ref()
            .and_then(|p| p.parent())
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let rx = cx.prompt_for_new_path(&dir, Some("Untitled.excalidraw"));
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                this.update_in(cx, |this, window, cx| this.write(path, window, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Runs `then` right away, or after the user agrees to drop unsaved changes.
    fn discard_guard(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if !self.board.read(cx).is_dirty() {
            return then(self, window, cx);
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "Discard unsaved changes?",
            None,
            &["Discard", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if matches!(answer.await, Ok(0)) {
                this.update_in(cx, |this, window, cx| then(this, window, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn open_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update_in(cx, |this, window, cx| this.load(Some(path), window, cx))
                    .ok();
            }
        })
        .detach();
    }
}

/// Serves the whiteboard's embedded toolbar icons (Zed serves the same paths from its assets).
struct Assets;

impl gpui::AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        Ok(whiteboard::icon_asset(path).map(std::borrow::Cow::Borrowed))
    }

    fn list(&self, _: &str) -> anyhow::Result<Vec<gpui::SharedString>> {
        Ok(Vec::new())
    }
}

fn show_error(e: anyhow::Error, window: &mut Window, cx: &mut App) {
    // Receiver dropped: the dialog only informs.
    let _ = window.prompt(PromptLevel::Critical, &format!("{e:#}"), None, &["OK"], cx);
}

impl Render for Root {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .on_action(cx.listener(|this, _: &New, window, cx| {
                this.discard_guard(window, cx, |this, window, cx| this.load(None, window, cx))
            }))
            .on_action(cx.listener(|this, _: &Open, window, cx| {
                this.discard_guard(window, cx, Self::open_dialog)
            }))
            .on_action(cx.listener(|this, _: &Save, window, cx| this.save(false, window, cx)))
            .on_action(cx.listener(|this, _: &SaveAs, window, cx| this.save(true, window, cx)))
            .on_action(cx.listener(|this, _: &Quit, window, cx| {
                this.discard_guard(window, cx, |_, _, cx| cx.quit())
            }))
            .child(self.board.clone())
    }
}

fn main() {
    application().with_assets(Assets).run(|cx: &mut App| {
        whiteboard::load_fonts(cx);
        cx.bind_keys([
            KeyBinding::new("ctrl-n", New, None),
            KeyBinding::new("ctrl-o", Open, None),
            KeyBinding::new("ctrl-s", Save, None),
            KeyBinding::new("ctrl-shift-s", SaveAs, None),
            KeyBinding::new("ctrl-q", Quit, None),
        ]);
        // ponytail: closing the window quits without the unsaved-changes prompt.
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        let path = std::env::args().nth(1).map(PathBuf::from);
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Whiteboard".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Root::new(path, window, cx)),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
