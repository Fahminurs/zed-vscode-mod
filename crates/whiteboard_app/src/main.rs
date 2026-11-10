#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Standalone whiteboard: real Excalidraw (bundled in `web/dist`) in a native window.
//! Usage: `whiteboard [file.excalidraw]`. The page talks to us through `window.ipc`.

use std::{borrow::Cow, path::PathBuf};

use include_dir::{Dir, include_dir};
use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
use serde_json::Value;
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use wry::{
    WebViewBuilder,
    http::{Response, header::CONTENT_TYPE},
};

static WEB: Dir = include_dir!("$CARGO_MANIFEST_DIR/web/dist");

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("woff2") => "font/woff2",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    }
}

fn respond(status: u16, mime: &str, body: Cow<'static, [u8]>) -> Response<Cow<'static, [u8]>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, mime)
        .body(body)
        .unwrap()
}

fn file_name(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .and_then(|p| p.file_name())
        .map_or("Untitled.excalidraw".into(), |n| {
            n.to_string_lossy().into_owned()
        })
}

fn title(path: &Option<PathBuf>, dirty: bool) -> String {
    format!(
        "{}{} - Whiteboard",
        file_name(path),
        if dirty { " •" } else { "" }
    )
}

fn file_dialog() -> FileDialog {
    FileDialog::new().add_filter("Excalidraw", &["excalidraw"])
}

fn discard_ok(dirty: bool) -> bool {
    !dirty
        || MessageDialog::new()
            .set_title("Whiteboard")
            .set_description("Discard unsaved changes?")
            .set_level(MessageLevel::Warning)
            .set_buttons(MessageButtons::YesNo)
            .show()
            == MessageDialogResult::Yes
}

fn error(e: impl ToString) {
    MessageDialog::new()
        .set_title("Whiteboard")
        .set_description(e.to_string())
        .set_level(MessageLevel::Error)
        .show();
}

fn main() -> wry::Result<()> {
    let mut path = std::env::args().nth(1).map(PathBuf::from);
    // Served once at /__file so the page can load it on start.
    let initial = path.as_ref().and_then(|p| std::fs::read(p).ok());

    let event_loop = EventLoopBuilder::<String>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title(title(&path, false))
        .with_inner_size(LogicalSize::new(1280.0, 800.0))
        .build(&event_loop)
        .expect("failed to open window");

    let webview = WebViewBuilder::new()
        .with_custom_protocol("wb".into(), move |_, request| {
            let p = request.uri().path().trim_start_matches('/');
            if p == "__file" {
                return match &initial {
                    Some(b) => respond(200, "application/json", Cow::Owned(b.clone())),
                    None => respond(404, "text/plain", Cow::Borrowed(&[])),
                };
            }
            let p = if p.is_empty() { "index.html" } else { p };
            match WEB.get_file(p) {
                Some(f) => respond(200, mime(p), Cow::Borrowed(f.contents())),
                None => respond(404, "text/plain", Cow::Borrowed(&[])),
            }
        })
        .with_ipc_handler(move |request| {
            let _ = proxy.send_event(request.into_body());
        })
        .with_background_color((18, 18, 18, 255))
        .with_url("wb://localhost/");
    // Measured: ~210 MB → ~115 MB. No GPU process (Excalidraw's 2D canvas renders fine in
    // software) + Chromium's low-memory mode. First flag keeps wry's own defaults.
    #[cfg(windows)]
    let webview = {
        use wry::WebViewBuilderExtWindows;
        webview.with_additional_browser_args(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
             --disable-gpu --enable-low-end-device-mode",
        )
    };
    let webview = webview.build(&window)?;

    let mut dirty = false;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(msg) => {
                let Ok(msg) = serde_json::from_str::<Value>(&msg) else {
                    return;
                };
                match msg["type"].as_str() {
                    Some("dirty") => dirty = msg["dirty"].as_bool().unwrap_or(false),
                    Some("save") => {
                        let target = path
                            .clone()
                            .filter(|_| msg["saveAs"].as_bool() != Some(true))
                            .or_else(|| file_dialog().set_file_name(file_name(&path)).save_file());
                        let Some(target) = target else { return };
                        match std::fs::write(&target, msg["data"].as_str().unwrap_or_default()) {
                            Ok(()) => {
                                path = Some(target);
                                dirty = false;
                                let _ = webview.evaluate_script("window.__wb.saved()");
                            }
                            Err(e) => error(e),
                        }
                    }
                    Some("open") => {
                        if !discard_ok(dirty) {
                            return;
                        }
                        let Some(p) = file_dialog().pick_file() else {
                            return;
                        };
                        match std::fs::read_to_string(&p) {
                            Ok(text) => {
                                let js = format!(
                                    "window.__wb.load({})",
                                    serde_json::to_string(&text).unwrap_or_default()
                                );
                                let _ = webview.evaluate_script(&js);
                                path = Some(p);
                                dirty = false;
                            }
                            Err(e) => error(e),
                        }
                    }
                    Some("new") => {
                        if discard_ok(dirty) {
                            let _ = webview.evaluate_script("window.__wb.reset()");
                            path = None;
                            dirty = false;
                        }
                    }
                    _ => return,
                }
                window.set_title(&title(&path, dirty));
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if discard_ok(dirty) {
                    *control_flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    });
}
