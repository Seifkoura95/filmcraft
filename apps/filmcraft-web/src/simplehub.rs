//! Simple Film inside SimpleHub. The page mounts the app on a canvas it owns and hands over a
//! bridge object that moves files to and from Simple Cloud:
//!
//! ```text
//! open(purpose?: string): Promise<{name: string, bytes: Uint8Array} | null>
//! save(name: string, bytes: Uint8Array): Promise<void>
//! ```
//!
//! File ▸ Open asks for `open("open")` (a `.fcproj`); imports ask with a purpose that says what
//! is wanted: `"media"` (movies, audio, stills and captions together: File ▸ Import, Link Media ▸
//! Locate…, Graphics ▸ New Layer From File), `"video"` (Attach Proxies, Reconnect Full
//! Resolution), `"picture"` (Import with Image Sequence), `"captions"` (Captions ▸ Import). The
//! bytes become a browser `File` and go down the same path as a picked file (range reads, OPFS
//! copy, retries while loading). Every file the app writes (Save, Save As, Save a Copy, exports,
//! captions, presets) goes to `save(name, bytes)`, fire and forget. Nothing here touches the
//! browser's file picker or downloads; drops on the canvas, the OPFS auto-save and recovery, the
//! `window.filmcraft` API and the audio worklet work as on the standalone page.
//!
//! The `AudioWorklet` script (`audio-worklet.js`) is loaded from next to the bundle:
//! `bridge.assetBase` when the host sets it, else [`ASSET_BASE`].
//!
//! An optional `theme` string on the bridge (`"dark"` or `"light"`) picks the UI theme the app
//! starts with; `FilmHandle::set_theme` switches it while the app runs. Both map onto the app's
//! own Settings ▸ Appearance ▸ Color Theme values (`"dark"` is the app's Darkest theme, whose
//! panels match the SimpleHub shell's window, `"light"` its Light theme) without saving them as
//! the user's preference.

use filmcraft_ui_egui::theme::ThemeKind;
use filmcraft_ui_egui::{FilmcraftApp, RelinkHint};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// Where SimpleHub serves the bundle (`trunk build --public-url /apps/film/`), for the files
/// loaded at run time.
pub const ASSET_BASE: &str = "/apps/film/";

/// A running instance. SimpleHub calls `destroy` when the module unmounts.
#[wasm_bindgen]
pub struct FilmHandle {
    runner: eframe::WebRunner,
}

#[wasm_bindgen]
impl FilmHandle {
    pub fn destroy(&self) {
        self.runner.destroy();
    }

    pub fn has_panicked(&self) -> bool {
        self.runner.has_panicked()
    }

    /// Switches the running app to `"dark"` or `"light"` on its next frame. Other values are
    /// ignored.
    pub fn set_theme(&self, theme: &str) {
        if let Some(k) = parse_theme(theme) {
            // `post` runs this before the frame's `logic` and asks for a repaint.
            crate::post(move |web, ctx| crate::apply_theme(web, ctx, k));
        }
    }
}

/// `"dark"` (or the app's own name for it, `"darkest"`) is the Darkest theme, the app's default,
/// whose `#1d1d1d` panels match the SimpleHub shell's window; `"light"` is the Light theme.
fn parse_theme(theme: &str) -> Option<ThemeKind> {
    match theme.trim().to_ascii_lowercase().as_str() {
        "dark" | "darkest" => Some(ThemeKind::Dark),
        "light" => Some(ThemeKind::Light),
        _ => None,
    }
}

/// The bridge's optional `theme` property.
fn bridge_theme(bridge: &JsValue) -> Option<ThemeKind> {
    js_sys::Reflect::get(bridge, &JsValue::from_str("theme")).ok().and_then(|v| v.as_string()).and_then(|s| parse_theme(&s))
}

/// Starts Simple Film on `canvas`, with `bridge` providing open and save. An optional `theme`
/// property (`"dark"` or `"light"`) on the bridge picks the initial theme; without it the app
/// keeps its own default.
#[wasm_bindgen]
pub async fn start_simple_film(canvas: web_sys::HtmlCanvasElement, bridge: JsValue) -> Result<FilmHandle, JsValue> {
    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    let theme = bridge_theme(&bridge);
    let mut base = js_sys::Reflect::get(&bridge, &"assetBase".into()).ok().and_then(|v| v.as_string()).unwrap_or_else(|| ASSET_BASE.into());
    if !base.ends_with('/') {
        base.push('/');
    }
    crate::audio::set_worklet_url(format!("{base}audio-worklet.js"));
    // Drops land on the app's canvas only: the rest of the SimpleHub page is not ours.
    crate::import::install_drop_handlers_on(&canvas)?;
    let save_bridge = bridge.clone();
    let host = crate::Host {
        on_write: Box::new(move |path: &str, data: &[u8]| {
            let bridge = save_bridge.clone();
            let name = path.rsplit('/').next().unwrap_or(path).to_string();
            let bytes = js_sys::Uint8Array::from(data);
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = call_save(&bridge, &name, &bytes).await {
                    log::error!("SimpleHub save of {name} failed: {e:?}");
                }
            });
        }),
        install_hooks: Box::new(move |app| install_hooks(app, bridge)),
        demo: true,
        cpu: false,
        webgl: false,
        recover: true,
        restore_media: true,
        theme,
    };
    let runner = eframe::WebRunner::new();
    crate::boot(&runner, canvas, host).await?;
    Ok(FilmHandle { runner })
}

/// The UI's file dialogs, through the bridge. The hooks are asynchronous: they start the request
/// and return nothing; the file is opened or imported when the host has delivered it.
fn install_hooks(app: &mut FilmcraftApp, bridge: JsValue) {
    let b = bridge.clone();
    app.hooks.pick_files = Some(Box::new(move |exts: &[&str]| {
        open_then(b.clone(), purpose_for(exts), None);
        Vec::new()
    }));
    let b = bridge.clone();
    app.hooks.pick_open_project = Some(Box::new(move || {
        open_then(b.clone(), "open", None);
        None
    }));
    // Saving writes the file in memory (`fs::WebServices`), which hands it to `save`.
    app.hooks.pick_save = Some(Box::new(|name: &str| Some(format!("/projects/{name}"))));
    app.hooks.pick_save_as = Some(Box::new(|_filter: &str, _exts: &[&str], name: &str| Some(format!("/exports/{name}"))));
    // Link Media ▸ Locate…, Attach Proxies, Reconnect Full Resolution: the hint's command runs
    // once the host has delivered the file (#111).
    app.hooks.pick_file_for_relink = Some(Box::new(move |exts: &[&str], hint: Option<RelinkHint>| {
        if hint.is_some() {
            open_then(bridge.clone(), purpose_for(exts), hint);
        }
        None
    }));
}

/// The `open` purpose for a picker's extension filter.
fn purpose_for(exts: &[&str]) -> &'static str {
    let any = |list: &[&str]| exts.iter().any(|e| list.contains(e));
    match (any(filmcraft_media::VIDEO_EXTENSIONS), any(filmcraft_media::AUDIO_EXTENSIONS), any(filmcraft_media::STILL_EXTENSIONS)) {
        (false, false, false) => "captions",
        (false, false, true) => "picture",
        (false, true, false) => "audio",
        (true, false, false) => "video",
        _ => "media",
    }
}

/// Ask the host for a file and hand it to the import path (or the relink hint's command).
fn open_then(bridge: JsValue, purpose: &'static str, relink: Option<RelinkHint>) {
    wasm_bindgen_futures::spawn_local(async move {
        match call_open(&bridge, purpose).await {
            Ok(Some(file)) => crate::import::picked(vec![file], relink),
            Ok(None) => {}
            Err(e) => log::error!("SimpleHub open({purpose}) failed: {e:?}"),
        }
    });
}

fn method(bridge: &JsValue, name: &str) -> Result<js_sys::Function, JsValue> {
    js_sys::Reflect::get(bridge, &JsValue::from_str(name))?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| JsValue::from_str(&format!("bridge.{name} is not a function")))
}

/// `bridge.open(purpose)`: the file the host delivered, as a browser `File` so it takes the same
/// path as a picked one (`None` when the user cancelled).
async fn call_open(bridge: &JsValue, purpose: &str) -> Result<Option<web_sys::File>, JsValue> {
    let promise: js_sys::Promise = method(bridge, "open")?.call1(bridge, &JsValue::from_str(purpose))?.dyn_into()?;
    let result = JsFuture::from(promise).await?;
    if result.is_null() || result.is_undefined() {
        return Ok(None);
    }
    let name = js_sys::Reflect::get(&result, &JsValue::from_str("name"))?.as_string().unwrap_or_else(|| "Untitled".into());
    let bytes: js_sys::Uint8Array = js_sys::Reflect::get(&result, &JsValue::from_str("bytes"))?.dyn_into()?;
    let parts = js_sys::Array::of1(&bytes);
    Ok(Some(web_sys::File::new_with_u8_array_sequence(&parts, &name)?))
}

async fn call_save(bridge: &JsValue, name: &str, bytes: &js_sys::Uint8Array) -> Result<(), JsValue> {
    let ret = method(bridge, "save")?.call2(bridge, &JsValue::from_str(name), bytes)?;
    if let Ok(promise) = ret.dyn_into::<js_sys::Promise>() {
        JsFuture::from(promise).await?;
    }
    Ok(())
}
