//! Geração de quadros no lado de quem assiste (assistir com mais fps que a stream).
//!
//! Entre cada par de quadros consecutivos (A, B), a NVIDIA Optical Flow (`nvof`) mede
//! para onde cada bloco da imagem andou, e um shader desenha a imagem em qualquer ponto
//! do caminho (fração `t` de 0 a 1), juntando A empurrada para frente e B puxada para
//! trás. Para assistir a k vezes o fps da stream, cada intervalo ganha k-1 quadros
//! gerados; o fluxo é calculado uma vez só por par. Custo: B aparece (k-1)/k de um
//! intervalo mais tarde (a 30 fps: ~16 ms para 60, ~26 ms para 144).
//!
//! Em cada ponto, o shader testa alguns movimentos candidatos (os vetores de ida e de
//! volta, o do bloco exato e "parado") e fica com o que faz A e B baterem melhor. Isso
//! resolve as bordas de objetos em movimento, onde só um dos dois campos tem o vetor
//! certo. Numa troca de cena (quase nada bate), o quadro do meio é pulado.

use crate::media::Nv12Frame;
use crate::nvof::{FlowField, FlowPair, OpticalFlow};
use std::time::Duration;

/// Intervalo entre quadros da stream que dá para interpolar (ms): até ~125 fps; abaixo de
/// ~14 fps o movimento entre quadros é grande demais para ficar bom.
const MIN_GAP_MS: f64 = 8.0;
const MAX_GAP_MS: f64 = 70.0;
/// Acima dessa fração de blocos inconsistentes, considera troca de cena.
const MAX_INCONSISTENCY: f32 = 0.35;

/// O que fazer com um quadro novo, dado o fps que quem assiste escolheu.
#[derive(Debug, PartialEq)]
pub enum Pacing {
    /// Mostra como veio.
    Show,
    /// Descarta (fps escolhido menor que o da stream).
    Skip,
    /// Mostra com `n` quadros por intervalo da stream (n-1 gerados + o real).
    Generate(u32),
}

/// Decide quadro a quadro como chegar ao fps escolhido por quem assiste.
pub struct Pacer {
    /// Próximo horário em que um quadro deve aparecer (ao reduzir o fps).
    due: Option<std::time::Instant>,
}

impl Pacer {
    pub fn new() -> Self {
        Self { due: None }
    }

    /// `at`: quando este quadro apareceria. `gap_ms`: intervalo desde o quadro anterior da
    /// stream. `target_fps`: o escolhido (0 = original).
    pub fn decide(&mut self, at: std::time::Instant, gap_ms: Option<f64>, target_fps: u32) -> Pacing {
        let (Some(gap), true) = (gap_ms.filter(|g| *g > 0.0), target_fps > 0) else {
            self.due = None;
            return Pacing::Show;
        };
        let target = Duration::from_secs_f64(1.0 / target_fps as f64);
        let gap_d = Duration::from_secs_f64(gap / 1000.0);
        if target.as_secs_f64() * 1000.0 > gap * 1.15 {
            // Menos fps que a stream: mostra o quadro mais perto de cada horário marcado,
            // e os horários andam de `target` em `target` (assim a média bate certinho,
            // em vez de "um sim, um não").
            return match self.due {
                Some(due) if at + gap_d / 2 < due => Pacing::Skip,
                Some(due) => {
                    self.due = Some(if at > due + target { at + target } else { due + target });
                    Pacing::Show
                }
                None => {
                    self.due = Some(at + target);
                    Pacing::Show
                }
            };
        }
        self.due = None;
        let n = (gap / (target.as_secs_f64() * 1000.0)).round() as u32;
        if n >= 2 { Pacing::Generate(n.min(8)) } else { Pacing::Show }
    }
}

/// Movimento entre o quadro mostrado agora (A) e o novo (B), e o intervalo entre eles.
pub struct Midpoint {
    pub flow: FlowPair,
    pub gap: Duration,
}

/// Roda na thread de decodificação de cada stream.
pub struct FrameGen {
    of: Option<OpticalFlow>,
    /// Sessão sendo criada em outra thread (tamanho e resposta). Criar leva ~170 ms: na
    /// thread de decodificação, isso parava a imagem e o buffer de reprodução achava que
    /// a rede tinha piorado (subia para 80 ms de atraso e levava uns 10 s para voltar).
    pending: Option<((u32, u32), std::sync::mpsc::Receiver<Result<OpticalFlow, String>>)>,
    broken: bool,
    prev_ts: Option<u32>,
}

/// Libera a sessão em outra thread (também demora).
fn drop_later(of: Option<OpticalFlow>) {
    if let Some(of) = of {
        std::thread::spawn(move || drop(of));
    }
}

impl FrameGen {
    pub fn new() -> Self {
        Self { of: None, pending: None, broken: false, prev_ts: None }
    }

    /// A sessão do tamanho `size`, se já estiver pronta (senão começa a criar).
    fn ready(&mut self, size: (u32, u32)) -> bool {
        if self.of.as_ref().is_some_and(|of| of.size() == size) {
            return true;
        }
        if !matches!(&self.pending, Some((s, _)) if *s == size) {
            drop_later(self.of.take());
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(OpticalFlow::new(size.0, size.1));
            });
            self.pending = Some((size, rx));
            return false;
        }
        let r = match &self.pending {
            Some((_, rx)) => match rx.try_recv() {
                Err(std::sync::mpsc::TryRecvError::Empty) => return false,
                Ok(r) => r,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Err("a criação da sessão falhou".into()),
            },
            None => return false,
        };
        self.pending = None;
        match r {
            Ok(of) => {
                self.of = Some(of);
                self.prev_ts = None;
                true
            }
            Err(e) => {
                eprintln!("geração de quadros desligada: {e}");
                self.broken = true;
                false
            }
        }
    }

    /// Quadro novo decodificado (timestamp RTP de 90 kHz). Devolve o movimento entre o
    /// anterior e este, quando dá para interpolar. Todo quadro mostrado tem que passar
    /// por aqui (o "anterior" daqui tem que ser o que está na tela).
    pub fn process(&mut self, f: &Nv12Frame, ts: u32) -> Option<Midpoint> {
        if self.broken || !self.ready((f.width, f.height)) {
            return None;
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
        Some(Midpoint { flow, gap: Duration::from_secs_f64(gap / 1000.0) })
    }

    /// Desligado pelo usuário: libera a placa e recomeça do zero quando ligar de novo.
    pub fn clear(&mut self) {
        drop_later(self.of.take());
        self.pending = None;
        self.prev_ts = None;
    }
}

/// Entrega cada quadro decodificado ao player no fps escolhido por quem assiste
/// (roda na thread de decodificação de cada stream).
pub struct Presenter {
    fg: FrameGen,
    pacer: Pacer,
    fg_active: bool,
    prev_ts: Option<u32>,
    /// Diagnóstico: maior tempo do gerador e maior atraso de chegada (ms) desde o último
    /// `take_stats`.
    proc_max: f64,
    late_max: f64,
}

impl Presenter {
    pub fn new() -> Self {
        Self { fg: FrameGen::new(), pacer: Pacer::new(), fg_active: false, prev_ts: None, proc_max: 0.0, late_max: 0.0 }
    }

    /// (maior tempo do gerador, maior atraso de chegada) em ms, e zera.
    pub fn take_stats(&mut self) -> (f64, f64) {
        let r = (self.proc_max, self.late_max);
        self.proc_max = 0.0;
        self.late_max = 0.0;
        r
    }

    /// `at`: quando o quadro deve aparecer. Espera até lá e entrega. Devolve quantos
    /// quadros vão para a tela (0 = descartado; n = n-1 gerados + o real).
    pub fn present(&mut self, f: Nv12Frame, ts: u32, at: std::time::Instant, view_fps: u32, feed: &crate::player::PlayerFeed) -> u32 {
        let gap_ms = self.prev_ts.map(|p| ts.wrapping_sub(p) as f64 / 90.0);
        self.prev_ts = Some(ts);
        let pacing = self.pacer.decide(at, gap_ms, view_fps);
        let wants = matches!(pacing, Pacing::Generate(_));
        if self.fg_active && !wants {
            self.fg.clear();
        }
        self.fg_active = wants;
        // Todo quadro mostrado passa pelo gerador enquanto ele está ativo (o "anterior"
        // dele tem que ser o que está na tela).
        let late = std::time::Instant::now().saturating_duration_since(at).as_secs_f64() * 1000.0;
        self.late_max = self.late_max.max(late);
        let t0 = std::time::Instant::now();
        let interp = if wants { self.fg.process(&f, ts) } else { None };
        self.proc_max = self.proc_max.max(t0.elapsed().as_secs_f64() * 1000.0);
        match (pacing, interp) {
            (Pacing::Skip, _) => 0,
            (Pacing::Generate(n), Some(m)) => {
                crate::playout::sleep_until(at);
                // n-1 quadros gerados ao longo do intervalo, depois o real.
                let base = at.max(std::time::Instant::now());
                let step = m.gap / n;
                let steps = (1..n).map(|j| (base + step * (j - 1), j as f32 / n as f32)).collect();
                feed.push_interp(f, m, steps, base + step * (n - 1));
                n
            }
            _ => {
                crate::playout::sleep_until(at);
                feed.push(f);
                1
            }
        }
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
// Fração do caminho de A até B (0 = A, 1 = B).
struct Params { t: f32, _p0: f32, _p1: f32, _p2: f32 };
@group(0) @binding(7) var<uniform> params: Params;

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

// Quanto A e B discordam se o movimento em `px` for `v` (A em px - t*v, B em
// px + (1-t)*v). Compara uma cruz de 5 pontos (um pixel só é ruidoso demais).
fn mismatch(px: vec2<f32>, v: vec2<f32>, size: vec2<f32>) -> f32 {
    let pa = px - params.t * v;
    let pb = px + (1.0 - params.t) * v;
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
    let t = params.t;
    let ca = rgb(a_y, a_uv, (px - t * best) / size);
    let cb = rgb(b_y, b_uv, (px + (1.0 - t) * best) / size);
    return vec4(mix(ca, cb, t), 1.0);
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
    params: wgpu::Buffer,
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
                wgpu::BindGroupLayoutEntry {
                    binding: 7,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
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
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("framegen t"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self { pipeline, layout, sampler, params, flows: None }
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

    /// Desenha o quadro na fração `t` do caminho entre `a` (mostrado agora) e `b` (o
    /// próximo). Um desenho por envio à fila (o `t` vai num buffer compartilhado).
    pub fn draw(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'_>,
        a: PlaneViews<'_>,
        b: PlaneViews<'_>,
        t: f32,
    ) -> bool {
        let Some((ab, ba, _, _)) = &self.flows else { return false };
        let mut p = [0u8; 16];
        p[..4].copy_from_slice(&t.clamp(0.0, 1.0).to_le_bytes());
        queue.write_buffer(&self.params, 0, &p);
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
                wgpu::BindGroupEntry { binding: 7, resource: self.params.as_entire_binding() },
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

    /// Simula `secs` segundos de uma stream a `src` fps vista a `target` fps; devolve
    /// (quadros mostrados por segundo, decisões).
    fn simulate(src: f64, target: u32, secs: f64) -> (f64, Vec<Pacing>) {
        let t0 = std::time::Instant::now();
        let mut p = Pacer::new();
        let gap = 1000.0 / src;
        let n = (src * secs) as usize;
        let mut shown = 0.0;
        let mut out = vec![];
        for i in 0..n {
            let at = t0 + Duration::from_secs_f64(i as f64 / src);
            let d = p.decide(at, (i > 0).then_some(gap), target);
            shown += match &d { Pacing::Show => 1.0, Pacing::Skip => 0.0, Pacing::Generate(k) => *k as f64 };
            out.push(d);
        }
        (shown / secs, out)
    }

    #[test]
    fn pacing_follows_the_chosen_fps() {
        let fps = |src, target| simulate(src, target, 10.0).0;
        let last = |src, target| simulate(src, target, 1.0).1.pop().unwrap();
        assert!((fps(30.0, 0) - 30.0).abs() < 1.0, "original");
        assert!((fps(30.0, 30) - 30.0).abs() < 1.0);
        assert_eq!(last(30.0, 60), Pacing::Generate(2));
        assert_eq!(last(30.0, 120), Pacing::Generate(4));
        assert_eq!(last(30.0, 144), Pacing::Generate(5));
        assert_eq!(last(60.0, 120), Pacing::Generate(2));
        assert_eq!(last(60.0, 144), Pacing::Generate(2));
        assert!((fps(30.0, 60) - 60.0).abs() < 2.0);
        // Reduzindo: a média tem que bater, não "um sim, um não".
        assert!((fps(60.0, 30) - 30.0).abs() < 1.0, "60 -> 30: {}", fps(60.0, 30));
        assert!((fps(144.0, 60) - 60.0).abs() < 1.5, "144 -> 60: {}", fps(144.0, 60));
        assert!((fps(144.0, 30) - 30.0).abs() < 1.0, "144 -> 30: {}", fps(144.0, 30));
        assert!((fps(120.0, 60) - 60.0).abs() < 1.0, "120 -> 60: {}", fps(120.0, 60));
    }

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
                on_packet: Arc::new(move |_, d, dur| { let _ = tx.send((d, dur)); }),
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
        // A sessão é criada em outra thread: espera ficar pronta.
        while !fg.ready((w, h)) {
            assert!(!fg.broken, "NVOF");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(fg.process(&a, 0).is_none());
        let mid = fg.process(&b, 3000).expect("quadro do meio (33 ms entre A e B)");
        assert!((mid.gap.as_secs_f64() * 1000.0 - 33.33).abs() < 0.1);

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
        let render = |t: f32| -> Vec<u8> {
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
                assert!(interp.draw(&device, &queue, &mut pass, PlaneViews { y: &ay, uv: &auv }, PlaneViews { y: &by, uv: &buv }, t));
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
            readback.unmap();
            px
        };

        // Um quarto do caminho: quadrado de x 116 a 276.
        let quarter = render(0.25);
        let lq = |x: u32| quarter[((180 * w + x) * 4) as usize];
        println!("t = 0,25: x=120 {}, x=272 {}, x=284 {}", lq(120), lq(272), lq(284));
        assert!(lq(120) > 180 && lq(272) > 180, "quadrado devia ir de 116 a 276 em t = 0,25");
        assert!(lq(284) < 140, "em t = 0,25 o quadrado ainda não chegou em x = 284");

        let px = render(0.5);
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
