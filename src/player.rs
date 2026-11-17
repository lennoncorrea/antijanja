//! Player nativo: desenha os quadros NV12 recebidos numa janela-filha dentro da janela do
//! app, com wgpu (DirectX 12 / Vulkan). A conversão NV12 → RGB (BT.709) é feita no shader.
//!
//! A janela-filha fica por cima da área de vídeo da interface; a interface (WebView) informa
//! o retângulo dessa área. Eventos de mouse/teclado na janela-filha viram eventos Tauri.

use crate::agenda::{Action, Agenda, Entry};
use crate::framegen::{Interpolator, Midpoint, PlaneViews};
use crate::media::Nv12Frame;
use std::num::NonZeroIsize;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use crate::app::AppHandle;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, HBRUSH, BLACK_BRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE, VK_F11};
use windows::Win32::UI::WindowsAndMessaging::*;

pub type Res<T> = Result<T, String>;
static APP: OnceLock<AppHandle> = OnceLock::new();
/// Janela nativa → ID da stream (para saber de qual tile veio o clique/tecla).
static TAGS: std::sync::Mutex<Option<std::collections::HashMap<isize, String>>> = std::sync::Mutex::new(None);

fn tag_of(hwnd: HWND) -> String {
    TAGS.lock().unwrap().as_ref().and_then(|m| m.get(&(hwnd.0 as isize)).cloned()).unwrap_or_default()
}
static LAST_MOVE: AtomicIsize = AtomicIsize::new(0);
static LAST_MOVE_HWND: AtomicIsize = AtomicIsize::new(0);
/// Mensagem do Windows quando o mouse sai da janela (pedida com `TrackMouseEvent`).
const WM_MOUSELEAVE: u32 = 0x02A3;
/// Janelas de vídeo com o cursor escondido (controles sumiram com o mouse parado).
static NO_CURSOR: std::sync::Mutex<Vec<isize>> = std::sync::Mutex::new(Vec::new());

/// Testes: (horário do present, gerado?, quanto esperou pelo get_current_texture).
#[cfg(test)]
pub static TRACE: std::sync::Mutex<Vec<(Instant, bool, Duration)>> = std::sync::Mutex::new(Vec::new());

enum Msg {
    Frame(Nv12Frame),
    /// Geração de quadros: entre o quadro atual e `next`, mostra o quadro gerado na fração
    /// `t` em cada horário de `steps`; depois `next` em `next_at`.
    Interp { next: Nv12Frame, mid: Midpoint, steps: Vec<(Instant, f32)>, next_at: Instant },
    Resize(u32, u32),
    Stop,
}

/// Player ativo. `push` pode ser chamado de qualquer thread.
pub struct Player {
    hwnd: isize,
    tx: SyncSender<Msg>,
    /// Raio dos cantos arredondados aplicado, junto com o tamanho da janela (a região é
    /// refeita se um dos dois mudar).
    corners: std::cell::Cell<Option<(i32, (i32, i32))>>,
    /// Posição atual (para não reposicionar a janela à toa a cada atualização da tela).
    rect: std::cell::Cell<Option<(i32, i32, i32, i32, bool)>>,
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let emit = |name: &str, kind: &str| {
        if let Some(app) = APP.get() {
            let _ = app.emit(name, serde_json::json!({ "id": tag_of(hwnd), "kind": kind }));
        }
    };
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONDOWN => {
            let _ = SetFocus(Some(hwnd));
            emit("player-input", "down");
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            emit("player-input", "dblclick");
            LRESULT(0)
        }
        WM_SETCURSOR if NO_CURSOR.lock().unwrap().contains(&(hwnd.0 as isize)) => {
            SetCursor(None);
            LRESULT(1)
        }
        WM_MOUSELEAVE => {
            emit("player-input", "leave");
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            // Avisa quando o mouse sair (WM_MOUSELEAVE).
            let mut track = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
            let _ = TrackMouseEvent(&mut track);
            // No máximo ~5 eventos por segundo (na hora, se o mouse passou para outro vídeo).
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as isize).unwrap_or(0);
            if now - LAST_MOVE.load(Ordering::Relaxed) > 200 || LAST_MOVE_HWND.load(Ordering::Relaxed) != hwnd.0 as isize {
                LAST_MOVE.store(now, Ordering::Relaxed);
                LAST_MOVE_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
                emit("player-input", "move");
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wp.0 as u16 == VK_ESCAPE.0 {
                emit("player-input", "escape");
            } else if wp.0 as u16 == VK_F11.0 {
                emit("player-input", "f11");
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn register_class() -> PCWSTR {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    let class = w!("AntiJanjaPlayer");
    REGISTERED.get_or_init(|| unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW {
            style: CS_DBLCLKS | CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
    class
}

impl Player {
    /// Cria a janela-filha e a thread de renderização. Chamar na thread principal.
    pub fn create(app: &AppHandle, parent: isize, tag: &str) -> Res<Player> {
        let _ = APP.set(app.clone());
        let p = Self::create_in(parent)?;
        TAGS.lock().unwrap().get_or_insert_with(Default::default).insert(p.hwnd, tag.to_string());
        Ok(p)
    }

    fn create_in(parent: isize) -> Res<Player> {
        let class = register_class();
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                w!("AntiJanja Player"),
                WS_CHILD | WS_CLIPSIBLINGS,
                0, 0, 16, 16,
                Some(HWND(parent as *mut _)),
                None,
                Some(GetModuleHandleW(None).map_err(|e| e.to_string())?.into()),
                None,
            )
            .map_err(|e| format!("CreateWindowEx: {e}"))?
        };
        let raw = hwnd.0 as isize;
        let (tx, rx) = sync_channel::<Msg>(4);
        let (ready_tx, ready_rx) = sync_channel::<Res<()>>(1);
        std::thread::Builder::new()
            .name("aj-player".into())
            .spawn(move || render_thread(raw, rx, ready_tx))
            .map_err(|e| e.to_string())?;
        ready_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "o player não iniciou".to_string())??;
        Ok(Player { hwnd: raw, tx, corners: std::cell::Cell::new(None), rect: std::cell::Cell::new(None) })
    }

    /// Arredonda os cantos da janela do vídeo (raio em pixels físicos; 0 = cantos retos),
    /// para acompanhar a borda arredondada do tile.
    pub fn set_corners(&self, radius: i32) {
        use windows::Win32::Graphics::Gdi::{CreateRoundRectRgn, SetWindowRgn};
        unsafe {
            let hwnd = HWND(self.hwnd as *mut _);
            let mut r = windows::Win32::Foundation::RECT::default();
            let _ = GetClientRect(hwnd, &mut r);
            let key = Some((radius, (r.right, r.bottom)));
            if self.corners.get() == key {
                return;
            }
            self.corners.set(key);
            if radius <= 0 {
                SetWindowRgn(hwnd, None, true);
            } else {
                let region = CreateRoundRectRgn(0, 0, r.right + 1, r.bottom + 1, radius * 2, radius * 2);
                // A janela passa a ser dona da região (não apagar).
                SetWindowRgn(hwnd, Some(region), true);
            }
        }
    }

    /// Esconde o cursor sobre o vídeo (controles sumiram com o mouse parado). Thread
    /// principal (a mesma da janela, para o `SetCursor` valer na hora).
    pub fn hide_cursor(&self, hide: bool) {
        {
            let mut list = NO_CURSOR.lock().unwrap();
            if list.contains(&self.hwnd) == hide {
                return;
            }
            if hide {
                list.push(self.hwnd);
            } else {
                list.retain(|h| *h != self.hwnd);
            }
        }
        unsafe {
            let mut p = windows::Win32::Foundation::POINT::default();
            if GetCursorPos(&mut p).is_ok() && WindowFromPoint(p).0 as isize == self.hwnd {
                SetCursor(if hide { None } else { LoadCursorW(None, IDC_ARROW).ok() });
            }
        }
    }

    /// Posiciona a área de vídeo (pixels físicos, relativos à janela). Thread principal.
    pub fn set_rect(&self, x: i32, y: i32, w: i32, h: i32, visible: bool) {
        if self.rect.get() == Some((x, y, w, h, visible)) {
            return;
        }
        self.rect.set(Some((x, y, w, h, visible)));
        unsafe {
            let hwnd = HWND(self.hwnd as *mut _);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                x, y, w.max(1), h.max(1),
                if visible { SWP_SHOWWINDOW | SWP_NOACTIVATE } else { SWP_HIDEWINDOW | SWP_NOACTIVATE },
            );
        }
        let _ = self.tx.try_send(Msg::Resize(w.max(1) as u32, h.max(1) as u32));
    }

    /// Entrega um quadro novo (se o player estiver atrasado, o quadro anterior é descartado).
    pub fn push(&self, frame: Nv12Frame) {
        match self.tx.try_send(Msg::Frame(frame)) {
            Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Ponto de entrada clonável para mandar quadros de outra thread.
    pub fn feed(&self) -> PlayerFeed {
        PlayerFeed { tx: self.tx.clone() }
    }

    /// Fecha o player. Thread principal.
    pub fn destroy(self) {
        if let Some(m) = TAGS.lock().unwrap().as_mut() {
            m.remove(&self.hwnd);
        }
        NO_CURSOR.lock().unwrap().retain(|h| *h != self.hwnd);
        let _ = self.tx.send(Msg::Stop);
        std::thread::sleep(Duration::from_millis(30));
        unsafe {
            let _ = DestroyWindow(HWND(self.hwnd as *mut _));
        }
    }
}

#[derive(Clone)]
pub struct PlayerFeed {
    tx: SyncSender<Msg>,
}

impl PlayerFeed {
    pub fn push(&self, frame: Nv12Frame) {
        let _ = self.tx.try_send(Msg::Frame(frame));
    }

    /// Quadros gerados entre o atual e `next` (`steps` = horário e fração de cada um),
    /// depois `next` em `next_at` (ver `framegen`).
    pub fn push_interp(&self, next: Nv12Frame, mid: Midpoint, steps: Vec<(Instant, f32)>, next_at: Instant) {
        let _ = self.tx.try_send(Msg::Interp { next, mid, steps, next_at });
    }
}

const SHADER: &str = r#"
struct Out { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -3.0), vec2(-1.0, 1.0), vec2(3.0, 1.0));
    var o: Out;
    o.pos = vec4(p[i], 0.0, 1.0);
    o.uv = vec2((p[i].x + 1.0) * 0.5, (1.0 - p[i].y) * 0.5);
    return o;
}

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_uv: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    // BT.709, faixa limitada (16-235)
    let y = (textureSample(tex_y, samp, in.uv).r - 0.0625) * 1.1644;
    let uv = textureSample(tex_uv, samp, in.uv).rg - vec2(0.5, 0.5);
    let r = y + 1.7927 * uv.y;
    let g = y - 0.2132 * uv.x - 0.5329 * uv.y;
    let b = y + 2.1124 * uv.x;
    return vec4(clamp(vec3(r, g, b), vec3(0.0), vec3(1.0)), 1.0);
}
"#;

/// Um quadro na placa de vídeo (planos Y e UV).
struct Planes {
    y: wgpu::Texture,
    uv: wgpu::Texture,
    y_view: wgpu::TextureView,
    uv_view: wgpu::TextureView,
    bind: wgpu::BindGroup,
    w: u32,
    h: u32,
}

struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Dois quadros: o mostrado (`cur`) e o próximo (para o quadro do meio).
    sets: [Option<Planes>; 2],
    cur: usize,
    interp: Interpolator,
}

fn init_gpu(hwnd: isize) -> Res<Gpu> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let handle = wgpu::rwh::Win32WindowHandle::new(NonZeroIsize::new(hwnd).ok_or("hwnd nulo")?);
    let surface = unsafe {
        instance
            .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(wgpu::rwh::RawDisplayHandle::Windows(wgpu::rwh::WindowsDisplayHandle::new())),
                raw_window_handle: wgpu::rwh::RawWindowHandle::Win32(handle),
            })
            .map_err(|e| format!("surface: {e}"))?
    };
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .map_err(|e| format!("adapter: {e}"))?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        .map_err(|e| format!("device: {e}"))?;

    let caps = surface.get_capabilities(&adapter);
    let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
    let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
        wgpu::PresentMode::Mailbox
    } else {
        wgpu::PresentMode::Fifo
    };
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: 16,
        height: 16,
        present_mode,
        desired_maximum_frame_latency: 1,
        alpha_mode: caps.alpha_modes[0],
        view_formats: vec![],
        color_space: wgpu::SurfaceColorSpace::Auto,
    };
    surface.configure(&device, &config);

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("nv12"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("nv12"),
        entries: &[
            tex_entry(0),
            tex_entry(1),
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("nv12"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("nv12"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs"), buffers: &[], compilation_options: Default::default() },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let interp = Interpolator::new(&device, format);
    Ok(Gpu { surface, device, queue, config, pipeline, layout, sampler, sets: [None, None], cur: 0, interp })
}

impl Gpu {
    fn resize(&mut self, w: u32, h: u32) {
        if (w, h) != (self.config.width, self.config.height) && w > 0 && h > 0 {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(&self.device, &self.config);
        }
    }

    /// Envia o quadro para o conjunto de texturas `slot`.
    fn upload(&mut self, f: &Nv12Frame, slot: usize) {
        let (w, h) = (f.width, f.height);
        if !matches!(&self.sets[slot], Some(p) if (p.w, p.h) == (w, h)) {
            let mk = |w, h, format| {
                self.device.create_texture(&wgpu::TextureDescriptor {
                    label: None,
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let y = mk(w, h, wgpu::TextureFormat::R8Unorm);
            let uv = mk(w / 2, h / 2, wgpu::TextureFormat::Rg8Unorm);
            let (y_view, uv_view) = (y.create_view(&Default::default()), uv.create_view(&Default::default()));
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&y_view) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&uv_view) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            });
            self.sets[slot] = Some(Planes { y, uv, y_view, uv_view, bind, w, h });
        }
        let p = self.sets[slot].as_ref().unwrap();
        let stride = f.stride;
        let y_size = (stride * h) as usize;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &p.y, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &f.data[..y_size],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &p.uv, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &f.data[y_size..],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: Some(h / 2) },
            wgpu::Extent3d { width: w / 2, height: h / 2, depth_or_array_layers: 1 },
        );
    }

    /// Quadro novo normal: substitui o mostrado.
    fn show(&mut self, f: &Nv12Frame) {
        self.upload(f, self.cur);
        self.render(None);
    }

    /// Prepara os quadros gerados entre o mostrado e `next` (que vai para o outro
    /// conjunto). Devolve false se não dá para interpolar (aí `next` já foi mostrado).
    fn prepare_interp(&mut self, next: &Nv12Frame, mid: &Midpoint) -> bool {
        let same_size = matches!(&self.sets[self.cur], Some(p) if (p.w, p.h) == (next.width, next.height));
        if !same_size {
            self.show(next);
            return false;
        }
        self.upload(next, 1 - self.cur);
        self.interp.upload_flow(&self.device, &self.queue, &mid.flow);
        true
    }

    /// O quadro preparado em `prepare_interp` passa a ser o mostrado.
    fn advance(&mut self) {
        self.cur = 1 - self.cur;
    }

    /// Desenha o quadro atual, ou (`t`) o gerado na fração `t` entre o atual e o outro.
    fn render(&mut self, t: Option<f32>) {
        #[cfg(test)]
        let waited = Instant::now();
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            _ => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(p) = &self.sets[self.cur] {
                // Mantém a proporção (barras pretas onde sobrar).
                let (sw, sh) = (self.config.width as f32, self.config.height as f32);
                let scale = (sw / p.w as f32).min(sh / p.h as f32);
                let (vw, vh) = (p.w as f32 * scale, p.h as f32 * scale);
                pass.set_viewport((sw - vw) / 2.0, (sh - vh) / 2.0, vw, vh, 0.0, 1.0);
                let drew_mid = match (t, &self.sets[1 - self.cur]) {
                    (Some(t), Some(next)) => self.interp.draw(
                        &self.device,
                        &self.queue,
                        &mut pass,
                        PlaneViews { y: &p.y_view, uv: &p.uv_view },
                        PlaneViews { y: &next.y_view, uv: &next.uv_view },
                        t,
                    ),
                    _ => false,
                };
                if !drew_mid {
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &p.bind, &[]);
                    pass.draw(0..3, 0..1);
                }
            }
        }
        #[cfg(test)]
        let waited = waited.elapsed();
        self.queue.submit([enc.finish()]);
        self.queue.present(frame);
        #[cfg(test)]
        TRACE.lock().unwrap().push((Instant::now(), t.is_some(), waited));
    }
}

fn render_thread(hwnd: isize, rx: Receiver<Msg>, ready: SyncSender<Res<()>>) {
    let mut gpu = match init_gpu(hwnd) {
        Ok(g) => {
            let _ = ready.send(Ok(()));
            g
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut agenda: Agenda<Nv12Frame, Midpoint> = Agenda::new();
    let trace = std::env::var_os("AJ_TRACE").is_some();
    let (mut t_win, mut n_real, mut n_gen, mut lag_max, mut render_max) = (Instant::now(), 0u32, 0u32, 0f64, 0f64);
    loop {
        if trace && t_win.elapsed() >= Duration::from_secs(1) {
            eprintln!(
                "[tela] reais {n_real}/s, gerados {n_gen}/s, gerados pulados {}, atraso máx do desenho {lag_max:.1} ms, desenho máx {render_max:.1} ms",
                agenda.skipped
            );
            agenda.skipped = 0;
            (t_win, n_real, n_gen, lag_max, render_max) = (Instant::now(), 0, 0, 0.0, 0.0);
        }
        let msg = match agenda.wait(Instant::now()) {
            Some(w) => match rx.recv_timeout(w) {
                Ok(m) => Some(m),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            },
        };
        let mut redraw = false;
        for m in msg.into_iter().chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            match m {
                Msg::Stop => return,
                Msg::Resize(w, h) => {
                    gpu.resize(w, h);
                    redraw = true;
                }
                Msg::Frame(f) => agenda.push(Entry::Frame(f)),
                Msg::Interp { next, mid, steps, next_at } => agenda.push(Entry::Pair { next, mid, steps, next_at }),
            }
        }
        match agenda.next(Instant::now()) {
            Action::Draw(step) => {
                if step.is_none() {
                    gpu.advance();
                }
                let t0 = Instant::now();
                if let Some(due) = agenda.last_due {
                    lag_max = lag_max.max(t0.saturating_duration_since(due).as_secs_f64() * 1000.0);
                }
                gpu.render(step);
                render_max = render_max.max(t0.elapsed().as_secs_f64() * 1000.0);
                if step.is_some() { n_gen += 1 } else { n_real += 1 }
            }
            Action::Show(f) => {
                let t0 = Instant::now();
                gpu.show(&f);
                render_max = render_max.max(t0.elapsed().as_secs_f64() * 1000.0);
                n_real += 1;
            }
            Action::Prepare(next, mid) => {
                let ok = gpu.prepare_interp(&next, &mid);
                agenda.prepared(ok, Instant::now());
            }
            Action::Advance => gpu.advance(),
            Action::Idle => {
                if redraw {
                    gpu.render(None);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pump(ms: u64) {
        let until = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < until {
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Caminho completo de quem assiste com fps escolhido: captura a 30 fps -> encoder ->
    /// decoder -> gerador de quadros -> player numa janela. Mede o intervalo entre os
    /// quadros que vão para a tela (engasgo = intervalo bem maior que o esperado).
    /// `cargo test framegen_smoothness -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn framegen_smoothness_end_to_end() {
        use crate::video::{Callbacks, VideoPipeline, VideoSettings};
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        use std::sync::Arc;
        crate::media::mf_init().unwrap();
        let parent = unsafe {
            CreateWindowExW(WS_EX_TOPMOST, w!("STATIC"), w!("teste fluidez"), WS_POPUP | WS_VISIBLE, 50, 50, 960, 540, None, None, None, None).unwrap()
        };
        pump(100);
        let player = Player::create_in(parent.0 as isize).expect("player");
        player.set_rect(0, 0, 960, 540, true);
        let feed = player.feed();
        let view_fps = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        // Simula a rede: um quadro em três chega 14 ms adiantado (chegadas em grupos).
        let jitter = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<(Vec<u8>, Duration)>();
        let src = crate::capture::list_sources().into_iter().find(|s| s.primary).unwrap();
        let pipe = VideoPipeline::start(src.id, VideoSettings { res: 1080, fps: 30, bitrate: 8000, cursor: true }, Callbacks {
            on_packet: Arc::new(move |_, d, dur, _| { let _ = tx.send((d, dur)); }),
            on_info: Arc::new(|_| {}), on_preview: Arc::new(|_| {}),
            on_error: Arc::new(|e| println!("ERRO {e}")), on_source_closed: Arc::new(|| {}),
        }).unwrap();
        let (vf, st, jt) = (view_fps.clone(), stop.clone(), jitter.clone());
        let late = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
        let lt = late.clone();
        let decode = std::thread::spawn(move || {
            crate::media::mf_init().unwrap();
            unsafe { windows::Win32::Media::timeBeginPeriod(1); }
            let mut dec = crate::media::Decoder::new().unwrap();
            let mut playout = crate::playout::Playout::new();
            let mut presenter = crate::framegen::Presenter::new();
            let mut ts = 0u32;
            let mut count = 0u64;
            while !st.load(Ordering::SeqCst) {
                let Ok((data, dur)) = rx.recv_timeout(Duration::from_millis(500)) else { continue };
                ts = ts.wrapping_add((dur.as_secs_f64() * 90000.0).round() as u32);
                for f in dec.decode(&data, 0).unwrap_or_default() {
                    count += 1;
                    let mut at = playout.schedule(ts);
                    if jt.load(Ordering::Relaxed) && count % 3 == 1 {
                        at -= Duration::from_millis(14);
                    }
                    presenter.present(f, ts, at, vf.load(Ordering::Relaxed), &feed);
                    // Quanto passou do horário ao entregar (o gerador roda antes da espera).
                    lt.lock().unwrap().push(Instant::now().saturating_duration_since(at).as_secs_f64() * 1000.0);
                }
            }
        });
        pump(1500);
        let mut results = vec![];
        for (target, irregular) in [(0u32, false), (60, false), (120, false), (60, true), (120, true)] {
            view_fps.store(target, Ordering::Relaxed);
            jitter.store(irregular, Ordering::Relaxed);
            pump(1000); // estabiliza
            TRACE.lock().unwrap().clear();
            late.lock().unwrap().clear();
            pump(5000);
            let trace = TRACE.lock().unwrap().clone();
            let iv: Vec<f64> = trace.windows(2).map(|w| (w[1].0 - w[0].0).as_secs_f64() * 1000.0).collect();
            let expect = 1000.0 / if target == 0 { 30.0 } else { target as f64 };
            let mean = iv.iter().sum::<f64>() / iv.len().max(1) as f64;
            let sd = (iv.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / iv.len().max(1) as f64).sqrt();
            let hitches = iv.iter().filter(|&&x| x > expect * 1.8).count();
            let max = iv.iter().cloned().fold(0.0, f64::max);
            let generated = trace.iter().filter(|t| t.1).count();
            let wait_max = trace.iter().map(|t| t.2.as_secs_f64() * 1000.0).fold(0.0, f64::max);
            let wait_avg = trace.iter().map(|t| t.2.as_secs_f64() * 1000.0).sum::<f64>() / trace.len().max(1) as f64;
            let lt = late.lock().unwrap();
            let late_max = lt.iter().cloned().fold(0.0, f64::max);
            results.push((target, irregular, hitches, trace.len() as f64 / 5.0));
            println!(
                "{}alvo {:>3}: {:.1} quadros/s na tela ({} gerados) | intervalo médio {:.2} ms (esperado {:.2}), desvio {:.2}, máx {:.1}, {} engasgos | espera da tela média {:.2} ms, máx {:.1} | decodificação atrasada até {:.1} ms",
                if irregular { "[chegadas irregulares] " } else { "" },
                if target == 0 { "orig".to_string() } else { target.to_string() },
                trace.len() as f64 / 5.0, generated, mean, expect, sd, max, hitches, wait_avg, wait_max, late_max
            );
        }
        stop.store(true, Ordering::SeqCst);
        pipe.stop();
        decode.join().unwrap();
        player.destroy();
        unsafe { let _ = DestroyWindow(parent); }
        for (target, irregular, hitches, fps) in results {
            if target > 0 {
                assert!(hitches <= 5, "alvo {target} (irregular: {irregular}): {hitches} engasgos");
                assert!(fps > target as f64 * 0.9, "alvo {target} (irregular: {irregular}): só {fps:.1} quadros/s");
            }
        }
    }

    /// Cria uma janela, encaixa o player e confere na tela se a cor do quadro aparece.
    #[test]
    #[ignore]
    fn renders_nv12_into_child_window() {
        let parent = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST, w!("STATIC"), w!("teste player"), WS_POPUP | WS_VISIBLE,
                100, 100, 640, 360, None, None, None, None,
            ).unwrap()
        };
        pump(100);
        let player = Player::create_in(parent.0 as isize).expect("player");
        player.set_rect(0, 0, 640, 360, true);
        // Quadro NV12 vermelho puro (BT.709 limitado: Y=63, U=102, V=240).
        let (w, h) = (320u32, 180u32);
        let mut data = vec![63u8; (w * h) as usize];
        for _ in 0..(w * h / 4) { data.push(102); data.push(240); }
        for _ in 0..30 {
            player.feed().push(Nv12Frame { width: w, height: h, stride: w, data: data.clone() });
            pump(16);
        }
        pump(300);
        let mon = xcap::Monitor::from_point(320, 280).unwrap();
        let (mx, my) = (mon.x().unwrap(), mon.y().unwrap());
        let img = mon.capture_region((420 - mx) as u32, (280 - my) as u32, 1, 1).unwrap();
        let p = img.get_pixel(0, 0);
        println!("pixel no centro do player: {:?}", p.0);
        assert!(p.0[0] > 180 && p.0[1] < 80 && p.0[2] < 80, "esperava vermelho, veio {:?}", p.0);

        // Cantos arredondados: no canto aparece a janela de trás, no meio continua o vídeo.
        player.set_corners(24);
        pump(200);
        let corner = mon.capture_region((100 + 1 - mx) as u32, (100 + 1 - my) as u32, 1, 1).unwrap().get_pixel(0, 0).0;
        let middle = mon.capture_region((420 - mx) as u32, (280 - my) as u32, 1, 1).unwrap().get_pixel(0, 0).0;
        println!("com cantos: canto {corner:?}, meio {middle:?}");
        assert!(!(corner[0] > 180 && corner[1] < 80), "o canto devia mostrar a janela de trás: {corner:?}");
        assert!(middle[0] > 180 && middle[1] < 80, "o meio continua o vídeo: {middle:?}");
        player.set_corners(0);
        pump(200);
        let back = mon.capture_region((100 + 1 - mx) as u32, (100 + 1 - my) as u32, 1, 1).unwrap().get_pixel(0, 0).0;
        assert!(back[0] > 180 && back[1] < 80, "sem cantos, o vídeo volta até a ponta: {back:?}");

        // Geração de quadros: o gerado na metade entre vermelho e azul (sem movimento) é a
        // mistura, e o azul aparece no horário marcado.
        let mut blue = vec![32u8; (w * h) as usize];
        for _ in 0..(w * h / 4) { blue.push(240); blue.push(118); }
        let zero = crate::nvof::FlowField { width: w / 4, height: h / 4, grid: 4, data: vec![0; (w / 4 * h / 4 * 2) as usize] };
        let mid = crate::framegen::Midpoint {
            flow: crate::nvof::FlowPair { forward: zero.clone(), backward: zero },
            gap: Duration::from_millis(800),
        };
        let now = Instant::now();
        player.feed().push_interp(Nv12Frame { width: w, height: h, stride: w, data: blue }, mid, vec![(now, 0.5)], now + Duration::from_millis(400));
        pump(150);
        let between = mon.capture_region((420 - mx) as u32, (280 - my) as u32, 1, 1).unwrap().get_pixel(0, 0).0;
        pump(600);
        let after = mon.capture_region((420 - mx) as u32, (280 - my) as u32, 1, 1).unwrap().get_pixel(0, 0).0;
        println!("quadro gerado (t = 0,5): {between:?}, depois: {after:?}");
        player.destroy();
        unsafe { let _ = DestroyWindow(parent); }
        assert!(between[0] > 70 && between[2] > 70, "o quadro gerado devia misturar vermelho e azul: {between:?}");
        assert!(after[2] > 180 && after[0] < 80, "depois devia ficar azul: {after:?}");
    }
}
