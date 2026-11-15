//! Fontes de captura (telas e janelas) e captura ao vivo com Windows Graphics Capture.

use base64::{engine::general_purpose::STANDARD, Engine};
use image::{codecs::jpeg::JpegEncoder, imageops, RgbaImage};
use serde::Serialize;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, MinimumUpdateIntervalSettings,
    SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

pub type Res<T> = Result<T, String>;

#[derive(Serialize, Clone)]
pub struct Source {
    /// "monitor:<hmonitor>" ou "window:<hwnd>"
    pub id: String,
    pub kind: &'static str,
    pub name: String,
    pub app: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    /// data URL JPEG
    pub thumbnail: String,
}

const THUMB_W: u32 = 360;
const HIDDEN_TITLES: &[&str] = &["Program Manager", "Windows Input Experience", "Microsoft Text Input Application", "Settings"];

pub fn jpeg_data_url(img: &RgbaImage, max_w: u32, quality: u8) -> String {
    let (w, h) = img.dimensions();
    let small = if w > max_w { imageops::thumbnail(img, max_w, (h * max_w / w.max(1)).max(1)) } else { img.clone() };
    let rgb = image::DynamicImage::ImageRgba8(small).to_rgb8();
    let mut out = Vec::new();
    if JpegEncoder::new_with_quality(&mut out, quality).encode_image(&rgb).is_err() {
        return String::new();
    }
    format!("data:image/jpeg;base64,{}", STANDARD.encode(out))
}

/// Handles de tela/janela (HMONITOR/HWND) podem ser capturados de qualquer thread.
struct SendWrap<T>(T);
unsafe impl<T> Send for SendWrap<T> {}

/// Lista telas e janelas com miniaturas (as capturas rodam em paralelo).
pub fn list_sources() -> Vec<Source> {
    let own_pid = std::process::id();
    let mut items: Vec<(Source, Box<dyn FnOnce() -> Option<RgbaImage> + Send>)> = Vec::new();

    if let Ok(monitors) = xcap::Monitor::all() {
        for (i, m) in monitors.into_iter().enumerate() {
            let Ok(id) = m.id() else { continue };
            let name = m.friendly_name().ok().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| format!("Tela {}", i + 1));
            let src = Source {
                id: format!("monitor:{id}"),
                kind: "monitor",
                name: format!("Tela {} ({name})", i + 1),
                app: String::new(),
                width: m.width().unwrap_or(0),
                height: m.height().unwrap_or(0),
                primary: m.is_primary().unwrap_or(false),
                thumbnail: String::new(),
            };
            let m = SendWrap(m);
            items.push((src, Box::new(move || { let m = m; m.0.capture_image().ok() })));
        }
    }

    if let Ok(windows) = xcap::Window::all() {
        for w in windows {
            let (Ok(id), Ok(title)) = (w.id(), w.title()) else { continue };
            let (width, height) = (w.width().unwrap_or(0), w.height().unwrap_or(0));
            if title.trim().is_empty()
                || HIDDEN_TITLES.contains(&title.as_str())
                || w.is_minimized().unwrap_or(true)
                || w.pid().unwrap_or(0) == own_pid
                || width < 160
                || height < 100
            {
                continue;
            }
            let app = w.app_name().unwrap_or_default();
            let src = Source {
                id: format!("window:{id}"),
                kind: "window",
                name: title,
                app,
                width,
                height,
                primary: false,
                thumbnail: String::new(),
            };
            let w = SendWrap(w);
            items.push((src, Box::new(move || { let w = w; w.0.capture_image().ok() })));
        }
    }

    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .into_iter()
            .map(|(mut src, grab)| {
                scope.spawn(move || {
                    if let Some(img) = grab() {
                        src.thumbnail = jpeg_data_url(&img, THUMB_W, 70);
                    }
                    src
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

/* ========================================================================== */
/*                                Captura ao vivo                               */
/* ========================================================================== */

pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    /// BGRA sem padding
    pub data: Vec<u8>,
}

pub enum CaptureMsg {
    Frame(RawFrame),
    /// Quadro que ficou na GPU (caminho zero-copy).
    Gpu(crate::gpu::GpuFrame),
    /// Quadro reduzido para a prévia (só no caminho da GPU).
    Preview(RawFrame),
    /// A janela fechou ou a captura parou sozinha.
    Closed,
}

struct Handler {
    tx: SyncSender<crate::video::Msg>,
    scratch: Vec<u8>,
    ring: Option<crate::gpu::SharedRing>,
    last_preview: std::time::Instant,
}

impl GraphicsCaptureApiHandler for Handler {
    /// (canal, usar o caminho da GPU)
    type Flags = (SyncSender<crate::video::Msg>, bool);
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let (tx, gpu) = ctx.flags;
        Ok(Self {
            tx,
            scratch: Vec::new(),
            ring: gpu.then(crate::gpu::SharedRing::new),
            last_preview: std::time::Instant::now() - std::time::Duration::from_secs(1),
        })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, control: InternalCaptureControl) -> Result<(), Self::Error> {
        use crate::video::Msg;
        if let Some(ring) = self.ring.as_mut() {
            let (w, h) = (frame.width(), frame.height());
            match ring.push(frame.device(), frame.device_context(), frame.as_raw_texture(), w, h) {
                Ok(Some(gf)) => {
                    if let Err(TrySendError::Disconnected(_)) = self.tx.try_send(Msg::Capture(CaptureMsg::Gpu(gf))) {
                        control.stop();
                        return Ok(());
                    }
                }
                Ok(None) => {} // encoder ocupado: descarta
                Err(_) => self.ring = None, // GPU falhou: segue pela CPU
            }
            if self.ring.is_some() {
                if self.last_preview.elapsed() >= std::time::Duration::from_millis(250) {
                    self.last_preview = std::time::Instant::now();
                    let fb = frame.buffer()?;
                    let (pw, ph) = (fb.width(), fb.height());
                    let data = fb.as_nopadding_buffer(&mut self.scratch);
                    if let Some(small) = crate::video::downscale(data, pw, ph, 640) {
                        let _ = self.tx.try_send(Msg::Capture(CaptureMsg::Preview(small)));
                    }
                }
                return Ok(());
            }
        }
        let fb = frame.buffer()?;
        let (width, height) = (fb.width(), fb.height());
        let data = fb.as_nopadding_buffer(&mut self.scratch).to_vec();
        match self.tx.try_send(crate::video::Msg::Capture(CaptureMsg::Frame(RawFrame { width, height, data }))) {
            Ok(()) | Err(TrySendError::Full(_)) => {} // encoder ocupado: descarta o quadro
            Err(TrySendError::Disconnected(_)) => control.stop(),
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        let _ = self.tx.try_send(crate::video::Msg::Capture(CaptureMsg::Closed));
        Ok(())
    }
}

/// Captura em andamento; para ao ser descartada.
pub struct Capture {
    control: Option<CaptureControl<Handler, Box<dyn std::error::Error + Send + Sync>>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(c) = self.control.take() {
            let _ = c.stop();
        }
    }
}

fn parse_id(id: &str) -> Res<(&str, usize)> {
    let (kind, raw) = id.split_once(':').ok_or("fonte inválida")?;
    let handle: usize = raw.parse().map_err(|_| "fonte inválida")?;
    Ok((kind, handle))
}

/// Começa a capturar `source_id`, mandando quadros para `tx`.
pub fn start(source_id: &str, cursor: bool, gpu: bool, tx: SyncSender<crate::video::Msg>) -> Res<Capture> {
    // Recursos que só existem em versões mais novas do Windows. Tenta do mais completo
    // ao mais simples, para nunca deixar de capturar por causa de um extra:
    // - sem borda amarela: Windows 11;
    // - sem limite de quadros (até 144 fps): Windows 11 24H2;
    // - controle do cursor: Windows 10 2004+.
    let cursor_opt = if cursor { CursorCaptureSettings::WithCursor } else { CursorCaptureSettings::WithoutCursor };
    let fast = MinimumUpdateIntervalSettings::Custom(std::time::Duration::from_millis(1));
    let attempts = [
        (cursor_opt, DrawBorderSettings::WithoutBorder, fast),
        (cursor_opt, DrawBorderSettings::WithoutBorder, MinimumUpdateIntervalSettings::Default),
        (cursor_opt, DrawBorderSettings::Default, fast),
        (cursor_opt, DrawBorderSettings::Default, MinimumUpdateIntervalSettings::Default),
        (CursorCaptureSettings::Default, DrawBorderSettings::Default, MinimumUpdateIntervalSettings::Default),
    ];
    let mut last_err = String::new();
    for (cur, border, interval) in attempts {
        match start_with(source_id, cur, border, gpu, tx.clone(), interval) {
            Ok(c) => return Ok(c),
            Err(e) if e.contains("fechada") || e.contains("inválida") => return Err(e),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn start_with(
    source_id: &str,
    cursor: CursorCaptureSettings,
    border: DrawBorderSettings,
    gpu: bool,
    tx: SyncSender<crate::video::Msg>,
    interval: MinimumUpdateIntervalSettings,
) -> Res<Capture> {
    let (kind, handle) = parse_id(source_id)?;
    macro_rules! go {
        ($item:expr) => {{
            let settings = Settings::new(
                $item,
                cursor,
                border,
                SecondaryWindowSettings::Default,
                interval,
                DirtyRegionSettings::Default,
                ColorFormat::Bgra8,
                (tx, gpu),
            );
            Handler::start_free_threaded(settings).map_err(|e| format!("Não foi possível capturar essa fonte: {e}"))?
        }};
    }
    let control = match kind {
        "monitor" => go!(Monitor::from_raw_hmonitor(handle as *mut std::ffi::c_void)),
        "window" => {
            let w = Window::from_raw_hwnd(handle as *mut std::ffi::c_void);
            if !w.is_valid() {
                return Err("Essa janela foi fechada.".into());
            }
            go!(w)
        }
        _ => return Err("fonte inválida".into()),
    };
    Ok(Capture { control: Some(control) })
}

#[allow(dead_code)]
pub fn unused(_: Receiver<()>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn lists_sources_with_thumbnails() {
        let t = std::time::Instant::now();
        let list = list_sources();
        println!("{} fontes em {:?}", list.len(), t.elapsed());
        for s in &list {
            println!("  {} | {} | {} | {}x{} | thumb {} KB", s.id, s.name, s.app, s.width, s.height, s.thumbnail.len() / 1024);
        }
        assert!(list.iter().any(|s| s.kind == "monitor" && !s.thumbnail.is_empty()));
    }
}
