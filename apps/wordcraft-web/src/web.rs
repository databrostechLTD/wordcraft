//! The browser shell: web `Services`, drag-and-drop, the unsaved-changes guard and the eframe
//! web runner.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wordcraft_engine::Session;
use wordcraft_ui_egui::{Inbox, Services, WordApp};

const DOC_EXTS: &[&str] = &["docx", "docm", "dotx", "dotm", "doc", "dot", "odt", "rtf", "txt", "md", "html", "htm", "tex", "json", "xml"];
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];
const RECIPIENT_EXTS: &[&str] = &["csv", "tsv", "txt", "xlsx", "xlsm", "ods"];
mod font_store;
const CANVAS_ID: &str = "wordcraft_canvas";
const LOADING_ID: &str = "wordcraft_loading";

pub fn start() {
    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    wasm_bindgen_futures::spawn_local(async {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            log::error!("no document");
            return;
        };
        let Some(canvas) = document.get_element_by_id(CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
            log::error!("missing <canvas id=\"{CANVAS_ID}\">");
            return;
        };
        let dirty = Rc::new(Cell::new(false));
        if let Err(e) = guard_unload(dirty.clone()) {
            log::error!("no unsaved-changes guard: {e}");
        }
        let mut options = eframe::WebOptions::default();
        if query().contains("webgl")
            && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
        {
            create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
        }
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                options,
                Box::new(move |cc| {
                    if let Some(rs) = &cc.wgpu_render_state {
                        log::info!("wordcraft-web: wgpu backend {:?}", rs.adapter.get_info().backend);
                    }
                    // Fonts the user added on an earlier visit (#582); the app notices them arrive.
                    font_store::load_all(cc.egui_ctx.clone());
                    let inbox: Inbox = Inbox::default();
                    if let Err(e) = listen_for_pasted_pictures(inbox.clone(), cc.egui_ctx.clone()) {
                        log::error!("pasting pictures is unavailable: {e}");
                    }
                    // An embedding host (e.g. the Nexa app) hands documents in and takes saves out over postMessage.
                    if let Err(e) = listen_for_host_messages(inbox.clone(), cc.egui_ctx.clone()) {
                        log::error!("host messaging is unavailable: {e}");
                    }
                    let embedded = query().contains("embedded");
                    let doc = if query().contains("sample") { wordcraft_engine::sample::sample_document() } else { wordcraft_doc::Document::new() };
                    let mut app = WordApp::new(Session::new(doc), services(inbox.clone(), cc.egui_ctx.clone(), dirty, embedded));
                    app.autosave = false;
                    Ok(Box::new(WebShell { app, inbox }))
                }),
            )
            .await;
        match result {
            Ok(()) => {
                if let Some(el) = document.get_element_by_id(LOADING_ID) {
                    el.remove();
                }
                // Tell the embedding host we are ready to receive a document.
                post_to_host("editor:ready", None, &[]);
            }
            Err(e) => {
                if let Some(el) = document.get_element_by_id(LOADING_ID) {
                    el.set_inner_html(&format!("<p>WordCraft failed to start: {e:?}</p><p>A browser with WebGPU or WebGL2 is required.</p>"));
                }
            }
        }
    });
}

fn query() -> String {
    web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
}

/// Wraps the app to read dropped files asynchronously (browsers can't read them synchronously)
/// and feed them through the inbox.
struct WebShell {
    app: WordApp,
    inbox: Inbox,
}

impl eframe::App for WebShell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let dropped = ctx.input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
        for f in dropped {
            let inbox = self.inbox.clone();
            let ctx = ctx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let name = f.path().file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "dropped".into());
                match f.bytes_async().await {
                    Ok(bytes) => {
                        inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
                        ctx.request_repaint();
                    }
                    Err(e) => log::error!("couldn't read dropped file {name}: {e}"),
                }
            });
        }
        self.app.logic(ctx);
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.app.raw_input_hook(raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }
}

/// Largest picture pasted from the browser's clipboard (as `insert.picture` allows).
const MAX_PASTED_BYTES: f64 = (200u32 << 20) as f64;
/// Most pictures one paste inserts.
const MAX_PASTED_PICTURES: u32 = 20;

/// Pasting a picture (#45): eframe's paste handler only reads text, so a clipboard holding just
/// pictures (a screenshot, a browser's Copy Image) is read here and the pictures arrive through
/// the inbox like dropped ones. A clipboard with text is left to eframe.
fn listen_for_pasted_pictures(inbox: Inbox, ctx: egui::Context) -> Result<(), String> {
    let document = web_sys::window().and_then(|w| w.document()).ok_or("no document")?;
    let on_paste = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::ClipboardEvent)>::new(move |e: web_sys::ClipboardEvent| {
        let Some(data) = e.clipboard_data() else { return };
        if !data.get_data("text").unwrap_or_default().is_empty() {
            return;
        }
        let Some(files) = data.files() else { return };
        for i in 0..files.length().min(MAX_PASTED_PICTURES) {
            let Some(file) = files.get(i) else { continue };
            if !file.type_().starts_with("image/") {
                continue;
            }
            if file.size() > MAX_PASTED_BYTES {
                log::warn!("pasted picture is larger than 200 MB; not inserted");
                continue;
            }
            let inbox = inbox.clone();
            let ctx = ctx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await {
                    Ok(buf) => {
                        let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                        // The inbox inserts files named like pictures; the engine reads the format from the bytes.
                        inbox.lock().unwrap_or_else(|e| e.into_inner()).push(("pasted.png".into(), bytes));
                        ctx.request_repaint();
                    }
                    Err(e) => log::error!("couldn't read the pasted picture: {e:?}"),
                }
            });
        }
    });
    document.add_event_listener_with_callback("paste", on_paste.as_ref().unchecked_ref()).map_err(|e| format!("{e:?}"))?;
    // The listener lives as long as the page.
    on_paste.forget();
    Ok(())
}

/// Largest document accepted from the embedding host, as a sanity cap.
const MAX_HOST_DOC_BYTES: u32 = 200u32 << 20;

/// An embedding host (such as the Nexa app) hands a document in and takes saves out over
/// `window.postMessage`. The host waits for an `editor:ready` message, then posts
/// `{ type: "editor:open", name, bytes }`, where `bytes` is an `ArrayBuffer` or `Uint8Array`.
fn listen_for_host_messages(inbox: Inbox, ctx: egui::Context) -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let on_message = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
        let data = e.data();
        let kind = js_sys::Reflect::get(&data, &wasm_bindgen::JsValue::from_str("type")).ok().and_then(|v| v.as_string()).unwrap_or_default();
        if kind != "editor:open" {
            return;
        }
        let name = js_sys::Reflect::get(&data, &wasm_bindgen::JsValue::from_str("name")).ok().and_then(|v| v.as_string()).unwrap_or_else(|| "document".into());
        let Ok(bytes_val) = js_sys::Reflect::get(&data, &wasm_bindgen::JsValue::from_str("bytes")) else {
            return;
        };
        if bytes_val.is_undefined() || bytes_val.is_null() {
            return;
        }
        let array = js_sys::Uint8Array::new(&bytes_val);
        if array.length() == 0 || array.length() > MAX_HOST_DOC_BYTES {
            return;
        }
        inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, array.to_vec()));
        ctx.request_repaint();
    });
    window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).map_err(|e| format!("{e:?}"))?;
    on_message.forget();
    Ok(())
}

/// Post a message to the embedding host (the parent window). When `bytes` is non-empty it is sent
/// as a `Uint8Array` under `bytes`, with the last component of `path` under `name`.
fn post_to_host(kind: &str, path: Option<&str>, bytes: &[u8]) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(Some(parent)) = window.parent() else {
        return;
    };
    let obj = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&obj, &wasm_bindgen::JsValue::from_str("type"), &wasm_bindgen::JsValue::from_str(kind));
    if let Some(path) = path {
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "document".into());
        let _ = js_sys::Reflect::set(&obj, &wasm_bindgen::JsValue::from_str("name"), &wasm_bindgen::JsValue::from_str(&name));
    }
    if !bytes.is_empty() {
        let _ = js_sys::Reflect::set(&obj, &wasm_bindgen::JsValue::from_str("bytes"), &js_sys::Uint8Array::from(bytes));
    }
    let _ = parent.post_message(&obj, "*");
}

/// `dirty` is the flag the `beforeunload` guard reads ([`guard_unload`]).
fn services(inbox: Inbox, ctx: egui::Context, dirty: Rc<Cell<bool>>, embedded: bool) -> Services {
    let open_inbox = inbox.clone();
    Services {
        open_async: Some(Box::new(move |purpose: &str| {
            let inbox = open_inbox.clone();
            let ctx = ctx.clone();
            let dialog = if purpose == "picture" {
                rfd::AsyncFileDialog::new().add_filter("Pictures", IMAGE_EXTS)
            } else if purpose == "font" {
                rfd::AsyncFileDialog::new().add_filter("Fonts", wordcraft_fonts::FONT_FILE_EXTS)
            } else if purpose == "recipients" {
                rfd::AsyncFileDialog::new().add_filter("Recipient lists", RECIPIENT_EXTS)
            } else {
                rfd::AsyncFileDialog::new().add_filter("Documents", DOC_EXTS)
            };
            wasm_bindgen_futures::spawn_local(async move {
                let Some(file) = dialog.pick_file().await else {
                    return;
                };
                let bytes = file.read().await;
                inbox.lock().unwrap_or_else(|e| e.into_inner()).push((file.file_name(), bytes));
                ctx.request_repaint();
            });
        })),
        // Exports ask for a name; the browser decides where the download goes.
        pick_save: Some(Box::new(|name: &str| Some(name.to_string()))),
        download: Some(Box::new(move |path: &str, bytes: &[u8]| {
            if embedded {
                // Embedded: hand the saved file back to the host instead of downloading it.
                post_to_host("editor:saved", Some(path), bytes);
                Ok(())
            } else {
                download(path, bytes)
            }
        })),
        print: Some(Box::new(print_pdf)),
        inbox: Some(inbox),
        on_dirty: Some(Box::new(move |d| dirty.set(d))),
        store_font: Some(Box::new(font_store::store)),
        forget_font: Some(Box::new(font_store::forget)),
        ..Default::default()
    }
}

/// Closing, reloading or leaving the tab with unsaved changes asks first. Browsers show their own
/// confirmation (a page can only ask for it), so there is no Save button: Cancel, then File › Save.
fn guard_unload(dirty: Rc<Cell<bool>>) -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let on_unload = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::BeforeUnloadEvent)>::new(move |e: web_sys::BeforeUnloadEvent| {
        if dirty.get() {
            e.prevent_default();
            // Older browsers ask only when `returnValue` is set.
            e.set_return_value("unsaved changes");
        }
    });
    window.add_event_listener_with_callback("beforeunload", on_unload.as_ref().unchecked_ref()).map_err(|e| format!("{e:?}"))?;
    // The listener lives as long as the page.
    on_unload.forget();
    Ok(())
}

/// Trigger a browser download of `bytes` named after the last component of `path`.
fn download(path: &str, bytes: &[u8]) -> Result<(), String> {
    let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "document".into());
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type(mime_for(&name));
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).map_err(js)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js)?;
    let a: web_sys::HtmlAnchorElement = document.create_element("a").map_err(js)?.dyn_into().map_err(|_| "not an anchor")?;
    a.set_href(&url);
    a.set_download(&name);
    a.style().set_property("display", "none").map_err(js)?;
    let body = document.body().ok_or("no body")?;
    body.append_child(&a).map_err(js)?;
    a.click();
    a.remove();
    // Revoke after the click has been dispatched; the download keeps its own reference.
    let revoke = wasm_bindgen::closure::Closure::once_into_js(move || {
        web_sys::Url::revoke_object_url(&url).ok();
    });
    window.set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 10_000).map_err(js)?;
    Ok(())
}

/// The hidden iframe the last print loaded its PDF into.
const PRINT_FRAME_ID: &str = "wordcraft-print-frame";

/// Opens `bytes` (a PDF) as an object URL in a hidden iframe and calls the
/// iframe's own `print()` once it has finished loading — this is the
/// standard way to drive the browser's native print dialog on a PDF without
/// a download or a popup window the browser might block. The previous
/// print's iframe and object URL are removed first, so only one copy of the
/// document's PDF stays in the page (the last one may still be printing).
fn print_pdf(bytes: &[u8]) -> Result<(), String> {
    let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;

    if let Some(old) = document.get_element_by_id(PRINT_FRAME_ID) {
        if let Ok(frame) = old.clone().dyn_into::<web_sys::HtmlIFrameElement>() {
            let src = frame.src();
            if src.starts_with("blob:") {
                web_sys::Url::revoke_object_url(&src).ok();
            }
        }
        old.remove();
    }

    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type("application/pdf");
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).map_err(js)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js)?;

    let iframe: web_sys::HtmlIFrameElement =
        match document.create_element("iframe").map_err(js).and_then(|e| e.dyn_into().map_err(|_| "not an iframe".to_string())) {
            Ok(f) => f,
            Err(e) => {
                web_sys::Url::revoke_object_url(&url).ok();
                return Err(e);
            }
        };
    iframe.set_id(PRINT_FRAME_ID);
    let attached = iframe
        .style()
        .set_property("display", "none")
        .map_err(js)
        .and_then(|()| document.body().ok_or_else(|| "no body".to_string()))
        .and_then(|body| body.append_child(&iframe).map_err(js));
    if let Err(e) = attached {
        iframe.remove();
        web_sys::Url::revoke_object_url(&url).ok();
        return Err(e);
    }

    // `onload` fires once the PDF has actually rendered inside the iframe;
    // printing before that would show a blank page. Set the handler BEFORE
    // `src` so a fast/cached load can't fire before we're listening.
    let iframe_for_load = iframe.clone();
    let on_load = wasm_bindgen::closure::Closure::once_into_js(move || {
        if let Some(w) = iframe_for_load.content_window() {
            // Ignore errors: some browsers (notably Firefox with a built-in
            // PDF viewer) print fine but report a benign cross-origin-style
            // error back on this call.
            let _ = w.print();
        }
    });
    iframe.set_onload(Some(on_load.unchecked_ref()));
    iframe.set_src(&url);
    Ok(())
}

fn mime_for(name: &str) -> &'static str {
    match name.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("pdf") => "application/pdf",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("odt") => "application/vnd.oasis.opendocument.text",
        Some("rtf") => "application/rtf",
        Some("html") => "text/html",
        Some("md" | "txt") => "text/plain",
        Some("tex") => "application/x-tex",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        _ => "application/octet-stream",
    }
}
