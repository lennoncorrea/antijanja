//! Player nativo: desenha os quadros NV12 recebidos numa janela-filha dentro da janela do
//! app, com wgpu (DirectX 12 / Vulkan). A conversão NV12 → RGB (BT.709) é feita no shader.
//!
//! A janela-filha fica por cima da área de vídeo da interface; a interface (WebView) informa
//! o retângulo dessa área. Eventos de mouse/teclado na janela-filha viram eventos Tauri.

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
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_F11};
use windows::Win32::UI::WindowsAndMessaging::*;

pub type Res<T> = Result<T, String>;
static APP: OnceLock<AppHandle> = OnceLock::new();
/// Janela nativa → ID da stream (para saber de qual tile veio o clique/tecla).
static TAGS: std::sync::Mutex<Option<std::collections::HashMap<isize, String>>> = std::sync::Mutex::new(None);

fn tag_of(hwnd: HWND) -> String {
    TAGS.lock().unwrap().as_ref().and_then(|m| m.get(&(hwnd.0 as isize)).cloned()).unwrap_or_default()
}
static LAST_MOVE: AtomicIsize = AtomicIsize::new(0);

enum Msg {
    Frame(Nv12Frame),
    Resize(u32, u32),
    Stop,
}

/// Player ativo. `push` pode ser chamado de qualquer thread.
pub struct Player {
    hwnd: isize,
    tx: SyncSender<Msg>,
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
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            emit("player-input", "dblclick");
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            // No máximo ~5 eventos por segundo.
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as isize).unwrap_or(0);
            if now - LAST_MOVE.load(Ordering::Relaxed) > 200 {
                LAST_MOVE.store(now, Ordering::Relaxed);
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
        Ok(Player { hwnd: raw, tx })
    }

    /// Posiciona a área de vídeo (pixels físicos, relativos à janela). Thread principal.
    pub fn set_rect(&self, x: i32, y: i32, w: i32, h: i32, visible: bool) {
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

struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    planes: Option<(wgpu::Texture, wgpu::Texture, wgpu::BindGroup, u32, u32)>,
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
    Ok(Gpu { surface, device, queue, config, pipeline, layout, sampler, planes: None })
}

impl Gpu {
    fn resize(&mut self, w: u32, h: u32) {
        if (w, h) != (self.config.width, self.config.height) && w > 0 && h > 0 {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(&self.device, &self.config);
        }
    }

    fn upload(&mut self, f: &Nv12Frame) {
        let (w, h) = (f.width, f.height);
        let recreate = !matches!(&self.planes, Some((_, _, _, pw, ph)) if (*pw, *ph) == (w, h));
        if recreate {
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
            let ty = mk(w, h, wgpu::TextureFormat::R8Unorm);
            let tuv = mk(w / 2, h / 2, wgpu::TextureFormat::Rg8Unorm);
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&ty.create_view(&Default::default())) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&tuv.create_view(&Default::default())) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            });
            self.planes = Some((ty, tuv, bind, w, h));
        }
        let (ty, tuv, _, _, _) = self.planes.as_ref().unwrap();
        let stride = f.stride;
        let y_size = (stride * h) as usize;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: ty, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &f.data[..y_size],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: tuv, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &f.data[y_size..],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: Some(h / 2) },
            wgpu::Extent3d { width: w / 2, height: h / 2, depth_or_array_layers: 1 },
        );
    }

    fn render(&mut self) {
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
            if let Some((_, _, bind, w, h)) = &self.planes {
                // Mantém a proporção (barras pretas onde sobrar).
                let (sw, sh) = (self.config.width as f32, self.config.height as f32);
                let scale = (sw / *w as f32).min(sh / *h as f32);
                let (vw, vh) = (*w as f32 * scale, *h as f32 * scale);
                pass.set_viewport((sw - vw) / 2.0, (sh - vh) / 2.0, vw, vh, 0.0, 1.0);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, bind, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([enc.finish()]);
        self.queue.present(frame);
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
    let mut last_render = Instant::now();
    while let Ok(msg) = rx.recv() {
        let mut latest: Option<Nv12Frame> = None;
        let mut redraw = false;
        let mut handle = |m: Msg, gpu: &mut Gpu, latest: &mut Option<Nv12Frame>| -> bool {
            match m {
                Msg::Frame(f) => *latest = Some(f),
                Msg::Resize(w, h) => {
                    gpu.resize(w, h);
                    redraw = true;
                }
                Msg::Stop => return false,
            }
            true
        };
        if !handle(msg, &mut gpu, &mut latest) {
            break;
        }
        // Junta mensagens pendentes: só o quadro mais novo importa.
        let mut stop = false;
        while let Ok(m) = rx.try_recv() {
            if !handle(m, &mut gpu, &mut latest) {
                stop = true;
                break;
            }
        }
        if stop {
            break;
        }
        if let Some(f) = latest {
            gpu.upload(&f);
            redraw = true;
        }
        if redraw {
            gpu.render();
            last_render = Instant::now();
        }
    }
    let _ = last_render;
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
        player.destroy();
        unsafe { let _ = DestroyWindow(parent); }
        assert!(p.0[0] > 180 && p.0[1] < 80 && p.0[2] < 80, "esperava vermelho, veio {:?}", p.0);
    }
}
