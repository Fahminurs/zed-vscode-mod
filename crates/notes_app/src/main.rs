#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Standalone live-preview Markdown editor.

mod editor;
mod markdown;

use std::path::PathBuf;

use editor::Editor;
use gpui::{
    App, Bounds, Context, Entity, Focusable, KeyBinding, PathPromptOptions, PromptLevel,
    TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div, prelude::*, px, size,
};
use gpui_platform::application;

actions!(notes, [New, Open, Save, SaveAs, Quit]);

struct Root {
    editor: Entity<Editor>,
}

impl Root {
    fn new(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(Editor::new);
        editor.focus_handle(cx).focus(window, cx);
        let mut this = Self { editor };
        this.load(path, window, cx);
        this
    }

    fn load(&mut self, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = self.editor.update(cx, |ed, cx| ed.open(path, cx)) {
            show_error(e, window, cx);
        }
    }

    fn write(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = self.editor.update(cx, |ed, cx| ed.save_to(path, cx)) {
            show_error(e, window, cx);
        }
    }

    fn save(&mut self, save_as: bool, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.editor.read(cx).path().cloned();
        if let Some(path) = current.clone().filter(|_| !save_as) {
            return self.write(path, window, cx);
        }
        let dir = current
            .as_ref()
            .and_then(|p| p.parent())
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let rx = cx.prompt_for_new_path(&dir, Some("Untitled.md"));
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
        if !self.editor.read(cx).is_dirty() {
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
            .child(self.editor.clone())
    }
}

fn main() {
    application().run(|cx: &mut App| {
        editor::bind_keys(cx);
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
        let bounds = Bounds::centered(None, size(px(1000.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Notes".into()),
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
