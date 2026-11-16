//! Geração de quadros no lado de quem assiste ("suavizar movimento").
//!
//! Com a stream a 30 fps, cada par de quadros consecutivos (A, B) ganha um quadro no
//! meio: a NVIDIA Optical Flow (`nvof`) mede para onde cada bloco da imagem andou, e um
//! shader desenha a imagem na metade do caminho, juntando A empurrada para frente e B
//! puxada para trás. O quadro do meio aparece quando B chegaria, e B meio intervalo
//! depois: ~16 ms a mais de atraso a 30 fps, em troca do dobro de fluidez.
//!
//! Em cada ponto, o shader testa alguns movimentos candidatos (os vetores de ida e de
//! volta, o do bloco exato e "parado") e fica com o que faz A e B baterem melhor. Isso
//! resolve as bordas de objetos em movimento, onde só um dos dois campos tem o vetor
//! certo. Numa troca de cena (quase nada bate), o quadro do meio é pulado.

use crate::media::Nv12Frame;
use crate::nvof::{FlowField, FlowPair, OpticalFlow};
use std::time::Duration;

/// Só interpola streams entre ~14 e ~45 fps (intervalo entre quadros em ms). Acima disso
/// o ganho é pequeno; abaixo, o movimento entre quadros é grande demais para ficar bom.
const MIN_GAP_MS: f64 = 22.0;
const MAX_GAP_MS: f64 = 70.0;
/// Acima dessa fração de blocos inconsistentes, considera troca de cena.
const MAX_INCONSISTENCY: f32 = 0.35;

/// Quadro intermediário pronto para o player: o fluxo entre o quadro mostrado agora e
/// `next`, e quanto tempo depois do quadro do meio o `next` deve aparecer.
pub struct Midpoint {
    pub flow: FlowPair,
    pub half_gap: Duration,
}

/// Roda na thread de decodificação de cada stream.
pub struct FrameGen {
    of: Option<OpticalFlow>,
    broken: bool,
    prev_ts: Option<u32>,
}

impl FrameGen {
    pub fn new() -> Self {
        Self { of: None, broken: false, prev_ts: None }
    }

    /// Quadro novo decodificado (timestamp RTP de 90 kHz). Devolve o quadro do meio entre
    /// o anterior e este, quando vale a pena.
    pub fn process(&mut self, f: &Nv12Frame, ts: u32) -> Option<Midpoint> {
        if self.broken {
            return None;
        }
        if self.of.as_ref().is_none_or(|of| of.size() != (f.width, f.height)) {
            self.of = None;
            match OpticalFlow::new(f.width, f.height) {
                Ok(of) => self.of = Some(of),
                Err(e) => {
                    eprintln!("geração de quadros desligada: {e}");
                    self.broken = true;
                    return None;
                }
            }
            self.prev_ts = None;
        }
        let gap_ms = self.prev_ts.map(|p| ts.wrapping_sub(p) as f64 / 90.0);
        self.prev_ts = Some(ts);
        let of = self.of.as_mut()?;
        let luma = f.data.get(..f.stride as usize * f.height as usize)?;
        let flow = match of.push(luma, f.stride as usize) {
            Ok(Some(p)) => p,
            Ok(None) => return None,
            Err(e) => {
                eprintln!("geração de quadros: {e}");
                of.reset();
                return None;
            }
        };
        let gap = gap_ms.filter(|g| (MIN_GAP_MS..=MAX_GAP_MS).contains(g))?;
        if flow.inconsistency() > MAX_INCONSISTENCY {
            return None;
        }
        Some(Midpoint { flow, half_gap: Duration::from_secs_f64(gap / 2000.0) })
    }

    /// Desligado pelo usuário: libera a placa e recomeça do zero quando ligar de novo.
    pub fn clear(&mut self) {
        self.of = None;
        self.prev_ts = None;
    }
}

/* ======================================================================== GPU */

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

@group(0) @binding(0) var a_y: texture_2d<f32>;
@group(0) @binding(1) var a_uv: texture_2d<f32>;
@group(0) @binding(2) var b_y: texture_2d<f32>;
@group(0) @binding(3) var b_uv: texture_2d<f32>;
// Vetores em S10.5 (pixels * 32), um por bloco de 4x4.
@group(0) @binding(4) var flow_ab: texture_2d<i32>;
@group(0) @binding(5) var flow_ba: texture_2d<i32>;
@group(0) @binding(6) var samp: sampler;

fn rgb(ty: texture_2d<f32>, tuv: texture_2d<f32>, uv: vec2<f32>) -> vec3<f32> {
    // BT.709, faixa limitada (16-235)
    let y = (textureSampleLevel(ty, samp, uv, 0.0).r - 0.0625) * 1.1644;
    let c = textureSampleLevel(tuv, samp, uv, 0.0).rg - vec2(0.5, 0.5);
    return clamp(vec3(y + 1.7927 * c.y, y - 0.2132 * c.x - 0.5329 * c.y, y + 2.1124 * c.x), vec3(0.0), vec3(1.0));
}

// Vetor (em pixels) no ponto `px`, interpolado entre os centros dos blocos.
fn flow(t: texture_2d<i32>, px: vec2<f32>) -> vec2<f32> {
    let dims = vec2<i32>(textureDimensions(t));
    let g = px / 4.0 - 0.5;
    let i0 = vec2<i32>(floor(g));
    let f = fract(g);
    let hi = dims - vec2(1, 1);
    let v00 = vec2<f32>(textureLoad(t, clamp(i0, vec2(0, 0), hi), 0).xy);
    let v10 = vec2<f32>(textureLoad(t, clamp(i0 + vec2(1, 0), vec2(0, 0), hi), 0).xy);
    let v01 = vec2<f32>(textureLoad(t, clamp(i0 + vec2(0, 1), vec2(0, 0), hi), 0).xy);
    let v11 = vec2<f32>(textureLoad(t, clamp(i0 + vec2(1, 1), vec2(0, 0), hi), 0).xy);
    return mix(mix(v00, v10, f.x), mix(v01, v11, f.x), f.y) / 32.0;
}

// Vetor do bloco exato (sem interpolar): na borda de um objeto em movimento, a média
// bilinear mistura o vetor do objeto com o do fundo e não serve para nenhum dos dois.
fn flow_block(t: texture_2d<i32>, px: vec2<f32>) -> vec2<f32> {
    let dims = vec2<i32>(textureDimensions(t));
    let p = clamp(vec2<i32>(px / 4.0), vec2(0, 0), dims - vec2(1, 1));
    return vec2<f32>(textureLoad(t, p, 0).xy) / 32.0;
}

fn luma(t: texture_2d<f32>, p: vec2<f32>, size: vec2<f32>) -> f32 {
    return textureSampleLevel(t, samp, p / size, 0.0).r;
}

// Quanto A e B discordam se o movimento em `px` for `v` (A em px - v/2, B em px + v/2).
// Compara uma cruz de 5 pontos (um pixel só é ruidoso demais).
fn mismatch(px: vec2<f32>, v: vec2<f32>, size: vec2<f32>) -> f32 {
    let pa = px - 0.5 * v;
    let pb = px + 0.5 * v;
    var e = 0.0;
    for (var i = 0; i < 5; i++) {
        var o = vec2(0.0, 0.0);
        if (i == 1) { o = vec2(-2.0, 0.0); }
        if (i == 2) { o = vec2(2.0, 0.0); }
        if (i == 3) { o = vec2(0.0, -2.0); }
        if (i == 4) { o = vec2(0.0, 2.0); }
        e += abs(luma(a_y, pa + o, size) - luma(b_y, pb + o, size));
    }
    return e;
}

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    let size = vec2<f32>(textureDimensions(a_y));
    let px = in.uv * size;
    // Candidatos ao movimento de A para B neste ponto. ida: A(p) = B(p + v_ab);
    // volta: B(p) = A(p + v_ba), então o movimento é -v_ba. Mais "parado" (texto e
    // interface fixos). Fica o que faz A e B baterem melhor: numa área que um objeto
    // acabou de descobrir ou cobrir, só um dos campos tem o vetor certo.
    var cands = array<vec2<f32>, 5>(
        flow(flow_ab, px),
        -flow(flow_ba, px),
        flow_block(flow_ab, px),
        -flow_block(flow_ba, px),
        vec2(0.0, 0.0),
    );
    var best = cands[0];
    var best_e = mismatch(px, best, size);
    for (var i = 1; i < 5; i++) {
        let e = mismatch(px, cands[i], size);
        if (e < best_e) {
            best_e = e;
            best = cands[i];
        }
    }
    let ca = rgb(a_y, a_uv, (px - 0.5 * best) / size);
    let cb = rgb(b_y, b_uv, (px + 0.5 * best) / size);
    return vec4(mix(ca, cb, 0.5), 1.0);
}
"#;

/// Planos Y e UV de um quadro já na placa de vídeo.
pub struct PlaneViews<'a> {
    pub y: &'a wgpu::TextureView,
    pub uv: &'a wgpu::TextureView,
}

/// Desenha o quadro do meio (precisa dos dois quadros e do fluxo já enviados).
pub struct Interpolator {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    flows: Option<(wgpu::Texture, wgpu::Texture, u32, u32)>,
}

impl Interpolator {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("framegen"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex = |binding, sample_type| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture { sample_type, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
            count: None,
        };
        let float = wgpu::TextureSampleType::Float { filterable: true };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("framegen"),
            entries: &[
                tex(0, float),
                tex(1, float),
                tex(2, float),
                tex(3, float),
                tex(4, wgpu::TextureSampleType::Sint),
                tex(5, wgpu::TextureSampleType::Sint),
                wgpu::BindGroupLayoutEntry {
                    binding: 6,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("framegen"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("framegen"),
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
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        Self { pipeline, layout, sampler, flows: None }
    }

    /// Envia os vetores de ida e volta para a placa.
    pub fn upload_flow(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, flow: &FlowPair) {
        let (w, h) = (flow.forward.width, flow.forward.height);
        if !matches!(&self.flows, Some((_, _, fw, fh)) if (*fw, *fh) == (w, h)) {
            let mk = || {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("flow"),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rg16Sint,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            self.flows = Some((mk(), mk(), w, h));
        }
        let (ab, ba, _, _) = self.flows.as_ref().unwrap();
        let write = |tex: &wgpu::Texture, f: &FlowField| {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                bytemuck_i16(&f.data),
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(f.width * 4), rows_per_image: Some(f.height) },
                wgpu::Extent3d { width: f.width, height: f.height, depth_or_array_layers: 1 },
            );
        };
        write(ab, &flow.forward);
        write(ba, &flow.backward);
    }

    /// Desenha o quadro do meio entre `a` (mostrado agora) e `b` (o próximo) no passo atual.
    pub fn draw(&self, device: &wgpu::Device, pass: &mut wgpu::RenderPass<'_>, a: PlaneViews<'_>, b: PlaneViews<'_>) -> bool {
        let Some((ab, ba, _, _)) = &self.flows else { return false };
        let (ab, ba) = (ab.create_view(&Default::default()), ba.create_view(&Default::default()));
        let entry = |binding, view| wgpu::BindGroupEntry { binding, resource: wgpu::BindingResource::TextureView(view) };
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("framegen"),
            layout: &self.layout,
            entries: &[
                entry(0, a.y),
                entry(1, a.uv),
                entry(2, b.y),
                entry(3, b.uv),
                entry(4, &ab),
                entry(5, &ba),
                wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.draw(0..3, 0..1);
        true
    }
}

fn bytemuck_i16(v: &[i16]) -> &[u8] {
    // i16 sem padding: reinterpretar como bytes é seguro (alinhamento de u8 é 1).
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Conteúdo real: captura a tela a 30 fps, codifica, decodifica e gera os quadros do
    /// meio, como acontece com quem assiste. Mede quantos saem e quanto custam.
    /// `cargo test framegen_real -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn framegen_real_screen_at_30fps() {
        use crate::video::{Callbacks, VideoPipeline, VideoSettings};
        use std::sync::{mpsc, Arc};
        use std::time::Instant;
        crate::media::mf_init().unwrap();
        let src = crate::capture::list_sources().into_iter().find(|s| s.primary).expect("sem tela");
        let (tx, rx) = mpsc::channel::<(Vec<u8>, Duration)>();
        let pipe = VideoPipeline::start(
            src.id,
            VideoSettings { res: 1080, fps: 30, bitrate: 8000, cursor: true },
            Callbacks {
                on_packet: Arc::new(move |d, dur| { let _ = tx.send((d, dur)); }),
                on_info: Arc::new(|_| {}),
                on_preview: Arc::new(|_| {}),
                on_error: Arc::new(|e| println!("ERRO {e}")),
                on_source_closed: Arc::new(|| {}),
            },
        )
        .unwrap();
        let mut dec = crate::media::Decoder::new().unwrap();
        let mut fg = FrameGen::new();
        let (mut ts, mut frames, mut mids, mut spent) = (0u32, 0u32, 0u32, Duration::ZERO);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(4) {
            let Ok((data, dur)) = rx.recv_timeout(Duration::from_secs(2)) else { break };
            ts = ts.wrapping_add((dur.as_secs_f64() * 90000.0).round() as u32);
            for f in dec.decode(&data, 0).unwrap_or_default() {
                frames += 1;
                let t = Instant::now();
                if fg.process(&f, ts).is_some() {
                    mids += 1;
                }
                spent += t.elapsed();
            }
        }
        pipe.stop();
        println!("{frames} quadros decodificados, {mids} quadros do meio gerados, {:?} por quadro", spent / frames.max(1));
        assert!(frames >= 90, "captura a 30 fps devia dar ~120 quadros em 4 s");
        assert!(mids as f32 >= frames as f32 * 0.8, "quase todo par de quadros devia ganhar um do meio");
        assert!((spent / frames.max(1)).as_millis() < 15);
    }

    /// Quadro NV12 sintético: fundo cinza com um quadrado branco em (x, y).
    fn frame_with_square(w: u32, h: u32, sx: u32, sy: u32) -> Nv12Frame {
        let mut data = vec![0u8; (w * h * 3 / 2) as usize];
        for y in 0..h {
            for x in 0..w {
                // Textura no fundo (o fluxo precisa de detalhe) + quadrado claro.
                let bg = 60 + ((x / 8 + y / 8) % 2) as u8 * 30 + (((x * 7) ^ (y * 13)) % 9) as u8;
                let inside = x >= sx && x < sx + 160 && y >= sy && y < sy + 160;
                // Textura sem repetição dentro do quadrado (listras periódicas enganariam o
                // fluxo: andar um múltiplo do período parece "não andou").
                let (qx, qy) = ((x.wrapping_sub(sx)) / 4, (y.wrapping_sub(sy)) / 4);
                let mut n = qx.wrapping_mul(374_761_393) ^ qy.wrapping_mul(668_265_263);
                n = (n ^ (n >> 13)).wrapping_mul(1_274_126_177);
                data[(y * w + x) as usize] = if inside { 190 + ((n >> 16) % 50) as u8 } else { bg };
            }
        }
        for v in &mut data[(w * h) as usize..] {
            *v = 128;
        }
        Nv12Frame { width: w, height: h, stride: w, data }
    }

    /// Placa NVIDIA de verdade: o quadrado anda 64 px entre A e B; o quadro do meio tem
    /// que mostrar o quadrado no meio (32 px), não dois quadrados apagados (mistura).
    /// Salva A, meio e B como PNG para conferir a olho.
    /// `cargo test framegen -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn midpoint_puts_the_moving_square_halfway() {
        let (w, h) = (640u32, 360u32);
        let a = frame_with_square(w, h, 100, 100);
        let b = frame_with_square(w, h, 164, 100);
        let mut fg = FrameGen::new();
        assert!(fg.process(&a, 0).is_none());
        let mid = fg.process(&b, 3000).expect("quadro do meio (33 ms entre A e B)");
        assert!((mid.half_gap.as_secs_f64() * 1000.0 - 16.67).abs() < 0.1);

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).unwrap();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut interp = Interpolator::new(&device, format);
        interp.upload_flow(&device, &queue, &mid.flow);

        let planes = |f: &Nv12Frame| {
            let mk = |w, h, format| {
                device.create_texture(&wgpu::TextureDescriptor {
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
            let (ty, tuv) = (mk(f.width, f.height, wgpu::TextureFormat::R8Unorm), mk(f.width / 2, f.height / 2, wgpu::TextureFormat::Rg8Unorm));
            let ys = (f.stride * f.height) as usize;
            queue.write_texture(ty.as_image_copy(), &f.data[..ys], wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(f.stride), rows_per_image: None }, ty.size());
            queue.write_texture(tuv.as_image_copy(), &f.data[ys..], wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(f.stride), rows_per_image: None }, tuv.size());
            (ty.create_view(&Default::default()), tuv.create_view(&Default::default()))
        };
        let (ay, auv) = planes(&a);
        let (by, buv) = planes(&b);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (w * h * 4) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = device.create_command_encoder(&Default::default());
        {
            let view = target.create_view(&Default::default());
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
            assert!(interp.draw(&device, &mut pass, PlaneViews { y: &ay, uv: &auv }, PlaneViews { y: &by, uv: &buv }));
        }
        enc.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo { buffer: &readback, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: None } },
            target.size(),
        );
        queue.submit([enc.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let px = readback.slice(..).get_mapped_range().unwrap().to_vec();

        let lum = |x: u32, y: u32| px[((y * w + x) * 4) as usize];
        // Quadrado do meio: x de 132 a 292. Centro dele claro; bordas que só A ou só B
        // teriam (x 110 e x 310) devem ser fundo, não meio-termo de mistura.
        let (center, only_a, only_b) = (lum(212, 180), lum(115, 180), lum(310, 180));
        println!("meio: centro {center}, onde só A tinha {only_a}, onde só B tem {only_b}");

        let dir = std::env::temp_dir().join("antijanja-framegen");
        let _ = std::fs::create_dir_all(&dir);
        let gray = |f: &Nv12Frame| image::GrayImage::from_raw(w, h, f.data[..(w * h) as usize].to_vec()).unwrap();
        gray(&a).save(dir.join("a.png")).unwrap();
        gray(&b).save(dir.join("b.png")).unwrap();
        image::RgbaImage::from_raw(w, h, px).unwrap().save(dir.join("meio.png")).unwrap();
        println!("imagens em {}", dir.display());

        assert!(center > 180, "o quadrado devia estar no meio do caminho");
        assert!(only_a < 140 && only_b < 140, "sobrou fantasma de A ou B (mistura em vez de movimento)");
    }
}
