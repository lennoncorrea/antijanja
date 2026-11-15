//! Pipeline de vídeo do host: captura → conversão/escala → encoder de hardware → pacotes.
//!
//! - Caminho principal 100% na GPU (zero-copy, ver `gpu`); se não der, cai para a CPU.
//! - Ritmo de quadros por prazo: entrega exatamente o FPS escolhido. Quadros que chegam
//!   alguns microssegundos adiantados não são descartados, e com a tela parada o último
//!   quadro é repetido (custa quase nada para o encoder e mantém o FPS estável).
//! - Comandos ao vivo: trocar a fonte, resolução, FPS, bitrate, cursor e pedir keyframe.

use crate::capture::{self, CaptureMsg, RawFrame};
use crate::gpu::{GpuConverter, GpuDevice, GpuFrame};
use crate::media::{adjust_bitrate, mf_init, rewrap, Converter, Encoder};
use image::RgbaImage;
use serde::Serialize;
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Media::MediaFoundation::IMFSample;

#[derive(Clone, Debug, PartialEq)]
pub struct VideoSettings {
    /// Altura máxima (0 = original).
    pub res: u32,
    pub fps: u32,
    /// kb/s (0 = automático).
    pub bitrate: u32,
    pub cursor: bool,
}

#[derive(Serialize, Clone, Default)]
pub struct VideoInfo {
    pub encoder: String,
    pub hardware: bool,
    /// Pipeline inteiro na GPU (sem cópia para a CPU).
    pub zero_copy: bool,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub kbps: u32,
    pub target_kbps: u32,
    /// Tempo médio de conversão + codificação por quadro (ms).
    pub encode_ms: f32,
    /// Quadros novos entregues pelo Windows por segundo (o resto são repetições).
    pub captured_fps: f32,
}

pub enum Msg {
    Capture(CaptureMsg),
    Settings(VideoSettings),
    Source(String),
    Keyframe,
    Stop,
}

pub struct Callbacks {
    /// (dados Annex B, duração do quadro)
    pub on_packet: Arc<dyn Fn(Vec<u8>, Duration) + Send + Sync>,
    pub on_info: Arc<dyn Fn(VideoInfo) + Send + Sync>,
    pub on_preview: Arc<dyn Fn(String) + Send + Sync>,
    pub on_error: Arc<dyn Fn(String) + Send + Sync>,
    pub on_source_closed: Arc<dyn Fn() + Send + Sync>,
}

#[derive(Clone)]
pub struct VideoPipeline {
    tx: SyncSender<Msg>,
}

impl VideoPipeline {
    pub fn start(source: String, settings: VideoSettings, cb: Callbacks) -> Result<Self, String> {
        let (tx, rx) = sync_channel::<Msg>(4);
        let tx2 = tx.clone();
        let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
        std::thread::Builder::new()
            .name("aj-video".into())
            .spawn(move || run(rx, tx2, source, settings, cb, ready_tx))
            .map_err(|e| e.to_string())?;
        ready_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "a captura não iniciou".to_string())??;
        Ok(Self { tx })
    }

    fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
    }
    pub fn set_settings(&self, s: VideoSettings) { self.send(Msg::Settings(s)) }
    pub fn set_source(&self, id: String) { self.send(Msg::Source(id)) }
    pub fn keyframe(&self) { self.send(Msg::Keyframe) }
    pub fn stop(&self) { self.send(Msg::Stop) }
}

fn even(v: u32) -> u32 {
    (v & !1).max(2)
}

fn output_size(w: u32, h: u32, max_h: u32) -> (u32, u32) {
    if max_h == 0 || max_h >= h {
        (even(w), even(h))
    } else {
        (even(w * max_h / h), even(max_h))
    }
}

fn auto_bitrate(w: u32, h: u32, fps: u32) -> u32 {
    // ~0,1 bit por pixel por quadro, entre 2 e 40 Mb/s.
    ((w as u64 * h as u64 * fps as u64) / 10).clamp(2_000_000, 40_000_000) as u32
}

fn bitrate_for(s: &VideoSettings, out: (u32, u32)) -> u32 {
    if s.bitrate > 0 { s.bitrate * 1000 } else { auto_bitrate(out.0, out.1, s.fps) }
}

enum Conv {
    Cpu(Converter),
    Gpu(GpuConverter),
}

impl Conv {
    fn input_size(&self) -> (u32, u32) {
        match self { Conv::Cpu(c) => c.input_size(), Conv::Gpu(c) => c.input_size() }
    }
    fn output_size(&self) -> (u32, u32) {
        match self { Conv::Cpu(c) => c.output_size(), Conv::Gpu(c) => c.output_size() }
    }
}

struct Active {
    conv: Conv,
    enc: Encoder,
    fps: u32,
    /// Bitrate pedido de fato ao encoder (corrigido pela malha fechada).
    effective: u32,
}

/// Último quadro recebido, ainda não convertido (CPU) ou já convertido (GPU).
enum Pending {
    Cpu(RawFrame),
    Gpu(GpuFrame),
}

struct State {
    settings: VideoSettings,
    gpu: Option<GpuDevice>,
    active: Option<Active>,
    want_keyframe: bool,
    /// Último NV12 codificado (para repetir com a tela parada).
    last_nv12: Option<IMFSample>,
    /// Tamanho do último quadro capturado.
    frame_size: Option<(u32, u32)>,
    start: Instant,
    convert_errors: u32,
    frame_index: u64,
}

impl State {
    /// Garante conversor + encoder para o tamanho de entrada atual.
    fn ensure(&mut self, input: (u32, u32), cb: &Callbacks) -> bool {
        let out = output_size(input.0, input.1, self.settings.res);
        let use_gpu = self.gpu.is_some();
        let ok = self.active.as_ref().is_some_and(|a| {
            a.conv.input_size() == input
                && a.conv.output_size() == out
                && a.fps == self.settings.fps
                && matches!(a.conv, Conv::Gpu(_)) == use_gpu
        });
        if ok {
            return true;
        }
        self.active = None;
        self.last_nv12 = None;
        let fps = self.settings.fps;
        let bitrate = bitrate_for(&self.settings, out);
        let built = match &self.gpu {
            Some(g) => GpuConverter::new(g, input.0, input.1, out.0, out.1, fps)
                .and_then(|c| Encoder::new(out.0, out.1, fps, bitrate, true, Some(&g.manager)).map(|e| (Conv::Gpu(c), e))),
            None => Converter::new(input.0, input.1, out.0, out.1, fps)
                .and_then(|c| Encoder::new(out.0, out.1, fps, bitrate, true, None).map(|e| (Conv::Cpu(c), e))),
        };
        match built {
            Ok((conv, enc)) => {
                self.active = Some(Active { conv, enc, fps, effective: bitrate });
                self.want_keyframe = true;
                true
            }
            Err(err) if self.gpu.is_some() => {
                // GPU não aceitou: segue pela CPU daqui pra frente.
                (cb.on_error)(format!("Caminho na GPU indisponível, usando a CPU ({err})"));
                self.gpu = None;
                false
            }
            Err(err) => {
                (cb.on_error)(format!("Erro no encoder: {err}"));
                false
            }
        }
    }

    fn timestamp(&mut self) -> (i64, i64) {
        let dur = (10_000_000 / self.settings.fps.max(1)) as i64;
        let t = self.frame_index as i64 * dur;
        self.frame_index += 1;
        (t, dur)
    }

    /// Converte o quadro pendente e codifica. Devolve os bytes enviados.
    fn encode_pending(&mut self, p: &mut Option<Pending>, cb: &Callbacks, interval: Duration) -> Option<usize> {
        let (t, dur) = self.timestamp();
        let nv12 = match p.take() {
            Some(Pending::Cpu(f)) => {
                if !self.ensure((f.width, f.height), cb) {
                    return None;
                }
                let Some(Active { conv: Conv::Cpu(c), .. }) = self.active.as_mut() else { return None };
                let r = c.convert(&f.data, t, dur);
                *p = Some(Pending::Cpu(f)); // guarda para a prévia
                r
            }
            Some(Pending::Gpu(f)) => {
                if !self.ensure((f.width, f.height), cb) {
                    return None;
                }
                let (Some(gpu), Some(Active { conv: Conv::Gpu(c), .. })) = (self.gpu.as_mut(), self.active.as_mut()) else { return None };
                c.convert(gpu, &f, t, dur)
            }
            None => match &self.last_nv12 {
                Some(s) => rewrap(s, t, dur),
                None => return None,
            },
        };
        let nv12 = match nv12 {
            Ok(s) => s,
            Err(err) => {
                // Falha pontual: pula este quadro. Muitas seguidas: recria o pipeline.
                self.convert_errors += 1;
                if self.convert_errors >= 30 {
                    (cb.on_error)(format!("Erro ao converter: {err}"));
                    self.active = None;
                    self.convert_errors = 0;
                }
                return None;
            }
        };
        self.convert_errors = 0;
        let a = self.active.as_mut()?;
        if self.want_keyframe {
            a.enc.force_keyframe();
            self.want_keyframe = false;
        }
        match a.enc.encode(&nv12) {
            Ok(packets) => {
                self.last_nv12 = Some(nv12);
                let mut bytes = 0;
                for (i, pk) in packets.into_iter().enumerate() {
                    bytes += pk.data.len();
                    (cb.on_packet)(pk.data, if i == 0 { interval } else { Duration::ZERO });
                }
                Some(bytes)
            }
            Err(err) => {
                (cb.on_error)(format!("Erro ao codificar: {err}"));
                self.active = None;
                None
            }
        }
    }
}

fn run(
    rx: std::sync::mpsc::Receiver<Msg>,
    tx: SyncSender<Msg>,
    mut source: String,
    settings: VideoSettings,
    cb: Callbacks,
    ready: SyncSender<Result<(), String>>,
) {
    if let Err(e) = mf_init() {
        let _ = ready.send(Err(e));
        return;
    }
    // Timer do Windows em 1 ms (o padrão de ~15,6 ms derruba o FPS das repetições).
    struct TimerRes;
    impl Drop for TimerRes {
        fn drop(&mut self) {
            unsafe { windows::Win32::Media::timeEndPeriod(1); }
        }
    }
    unsafe { windows::Win32::Media::timeBeginPeriod(1); }
    let _timer = TimerRes;
    let gpu = GpuDevice::new().ok();
    let mut capture = match capture::start(&source, settings.cursor, gpu.is_some(), tx.clone()) {
        Ok(c) => {
            let _ = ready.send(Ok(()));
            Some(c)
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // A prévia (redução + JPEG) roda numa thread à parte para não atrasar o vídeo.
    let (preview_tx, preview_rx) = sync_channel::<RawFrame>(1);
    {
        let on_preview = cb.on_preview.clone();
        std::thread::Builder::new()
            .name("aj-preview".into())
            .spawn(move || {
                while let Ok(f) = preview_rx.recv() {
                    let small = if f.width > 640 { downscale(&f.data, f.width, f.height, 640) } else { Some(f) };
                    if let Some(img) = small.as_ref().and_then(to_image) {
                        on_preview(capture::jpeg_data_url(&img, 640, 72));
                    }
                }
            })
            .ok();
    }
    let mut st = State { settings, gpu, active: None, want_keyframe: true, last_nv12: None, frame_size: None, start: Instant::now(), convert_errors: 0, frame_index: 0 };
    let mut pending: Option<Pending> = None;
    let mut next_due = Instant::now();
    // Relógio de vídeo: cada tick é 1/fps. A duração de cada quadro no RTP = ticks avançados,
    // assim quem assiste reproduz no ritmo certo mesmo se o host atrasar um quadro.
    let mut clock0 = next_due;
    let mut last_tick: u64 = 0;
    let mut clock_fps = st.settings.fps;
    let mut last_preview = Instant::now() - Duration::from_secs(10);
    let (mut win_frames, mut win_bytes, mut win_start) = (0u32, 0usize, Instant::now());
    let mut win_time = Duration::ZERO;
    let mut fresh_count = 0u32;

    loop {
        let interval = Duration::from_secs_f64(1.0 / st.settings.fps.max(1) as f64);
        if st.settings.fps != clock_fps {
            clock_fps = st.settings.fps;
            clock0 = Instant::now();
            next_due = clock0;
            last_tick = 0;
        }
        // Quadro novo esperando: codifica no tick. Sem quadro novo: só repete se ele estiver
        // atrasado meio intervalo (evita repetir e logo depois pular quando o Windows atrasa 1–2 ms).
        let wake_at = if pending.is_some() {
            Some(next_due)
        } else if st.last_nv12.is_some() {
            Some(next_due + interval / 2)
        } else {
            None
        };
        let timeout = match wake_at {
            Some(t) => t.saturating_duration_since(Instant::now()),
            None => Duration::from_millis(250),
        };

        let mut fresh = false;
        match rx.recv_timeout(timeout) {
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Msg::Capture(CaptureMsg::Closed)) => {
                (cb.on_source_closed)();
                capture = None;
                continue;
            }
            Ok(Msg::Capture(CaptureMsg::Frame(f))) => {
                st.frame_size = Some((f.width, f.height));
                pending = Some(Pending::Cpu(f));
                fresh = true;
                fresh_count += 1;
            }
            Ok(Msg::Capture(CaptureMsg::Gpu(f))) => {
                st.frame_size = Some((f.width, f.height));
                if st.gpu.is_none() {
                    continue; // quadro de GPU depois de cair para a CPU: ignora até reiniciar a captura
                }
                // Um quadro mais novo substitui o pendente: devolve o slot antigo à captura.
                if let (Some(Pending::Gpu(old)), Some(gpu)) = (&pending, st.gpu.as_mut()) {
                    gpu.discard(old);
                }
                pending = Some(Pending::Gpu(f));
                fresh_count += 1;
                fresh = true;
            }
            Ok(Msg::Capture(CaptureMsg::Preview(f))) => {
                let _ = preview_tx.try_send(f);
                continue;
            }
            Ok(Msg::Settings(s)) => {
                let restart_capture = s.cursor != st.settings.cursor;
                let bitrate_only = s.res == st.settings.res && s.fps == st.settings.fps;
                st.settings = s;
                if restart_capture {
                    capture = None;
                    match capture::start(&source, st.settings.cursor, st.gpu.is_some(), tx.clone()) {
                        Ok(c) => capture = Some(c),
                        Err(e) => (cb.on_error)(e),
                    }
                }
                match (st.active.as_mut(), bitrate_only) {
                    (Some(a), true) => {
                        a.effective = bitrate_for(&st.settings, a.conv.output_size());
                        a.enc.set_bitrate(a.effective);
                    }
                    _ => {
                        st.active = None;
                        st.last_nv12 = None;
                    }
                }
                st.want_keyframe = true;
                next_due = Instant::now();
                continue;
            }
            Ok(Msg::Source(id)) => {
                capture = None;
                match capture::start(&id, st.settings.cursor, st.gpu.is_some(), tx.clone()) {
                    Ok(c) => {
                        capture = Some(c);
                        source = id;
                        pending = None;
                        st.last_nv12 = None;
                        st.want_keyframe = true;
                    }
                    Err(e) => (cb.on_error)(e),
                }
                continue;
            }
            Ok(Msg::Keyframe) => {
                st.want_keyframe = true;
                next_due = Instant::now();
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        // Prazo: quadros com até 1/4 de intervalo de adiantamento entram.
        let now = Instant::now();
        let due = if pending.is_some() {
            now + interval / 4 >= next_due
        } else {
            now + Duration::from_micros(500) >= next_due + interval / 2
        };
        if !due {
            continue;
        }
        if !fresh && pending.is_none() && st.last_nv12.is_none() {
            continue;
        }
        let was_gpu = matches!(pending, Some(Pending::Gpu(_)));
        let tick = (next_due.saturating_duration_since(clock0).as_secs_f64() / interval.as_secs_f64()).round() as u64;
        let ticks = tick.saturating_sub(last_tick).max(1);
        let rtp_dur = interval * ticks as u32;
        let t0 = Instant::now();
        let result = st.encode_pending(&mut pending, &cb, rtp_dur);
        if result.is_some() {
            last_tick = tick.max(last_tick + 1);
        }
        win_time += t0.elapsed();
        if let Some(bytes) = result {
            win_frames += 1;
            win_bytes += bytes;
        } else if was_gpu && st.gpu.is_none() {
            // A GPU falhou: reinicia a captura pelo caminho da CPU.
            capture = None;
            match capture::start(&source, st.settings.cursor, false, tx.clone()) {
                Ok(c) => capture = Some(c),
                Err(e) => (cb.on_error)(e),
            }
        }
        next_due = if now > next_due + interval { now + interval } else { next_due + interval };

        // Prévia no caminho da CPU (no da GPU ela vem da captura).
        if let Some(Pending::Cpu(f)) = &pending {
            if last_preview.elapsed() >= Duration::from_millis(240) {
                last_preview = Instant::now();
                if let Some(small) = downscale(&f.data, f.width, f.height, 640) {
                    let _ = preview_tx.try_send(small);
                }
            }
            pending = None;
        }

        let el = win_start.elapsed();
        if el >= Duration::from_secs(1) {
            if let Some(a) = st.active.as_mut() {
                let out = a.conv.output_size();
                // Malha fechada: corrige o pedido ao encoder com base no que saiu.
                let measured = (win_bytes as f64 * 8.0 / el.as_secs_f64()) as u32;
                let target = bitrate_for(&st.settings, out);
                let next = adjust_bitrate(a.effective, target, measured);
                if next.abs_diff(a.effective) > a.effective / 50 {
                    a.effective = next;
                    a.enc.set_bitrate(next);
                }
                (cb.on_info)(VideoInfo {
                    encoder: a.enc.name.clone(),
                    hardware: a.enc.hardware,
                    zero_copy: matches!(a.conv, Conv::Gpu(_)),
                    width: out.0,
                    height: out.1,
                    fps: win_frames as f32 / el.as_secs_f32(),
                    kbps: (win_bytes as f64 * 8.0 / el.as_secs_f64() / 1000.0) as u32,
                    target_kbps: bitrate_for(&st.settings, out) / 1000,
                    captured_fps: fresh_count as f32 / el.as_secs_f32(),
                    encode_ms: if win_frames > 0 { win_time.as_secs_f32() * 1000.0 / win_frames as f32 } else { 0.0 },
                });
            }
            win_frames = 0;
            win_bytes = 0;
            win_time = Duration::ZERO;
            fresh_count = 0;
            win_start = Instant::now();
        }
    }
    drop(capture);
}

/// Reduz BGRA para no máximo `max_w` de largura (amostragem simples, barato).
pub fn downscale(bgra: &[u8], w: u32, h: u32, max_w: u32) -> Option<RawFrame> {
    let step = (w / max_w).max(1);
    let (ow, oh) = ((w / step).max(1), (h / step).max(1));
    let mut data = Vec::with_capacity((ow * oh * 4) as usize);
    for y in 0..oh {
        for x in 0..ow {
            let i = (((y * step) * w + x * step) * 4) as usize;
            data.extend_from_slice(bgra.get(i..i + 4)?);
        }
    }
    Some(RawFrame { width: ow, height: oh, data })
}

fn to_image(f: &RawFrame) -> Option<RgbaImage> {
    let mut img = RgbaImage::new(f.width, f.height);
    for (i, px) in img.pixels_mut().enumerate() {
        let p = f.data.get(i * 4..i * 4 + 4)?;
        *px = image::Rgba([p[2], p[1], p[0], 255]);
    }
    Some(img)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Captura a tela principal de verdade e confere FPS, bitrate e o caminho usado.
    #[test]
    #[ignore]
    fn capture_primary_monitor() {
        let src = capture::list_sources().into_iter().find(|s| s.primary || s.kind == "monitor").expect("sem tela");
        println!("capturando {} ({}x{})", src.name, src.width, src.height);
        let packets = Arc::new(AtomicUsize::new(0));
        let infos = Arc::new(std::sync::Mutex::new(Vec::<VideoInfo>::new()));
        let (p, inf) = (packets.clone(), infos.clone());
        let pipe = VideoPipeline::start(
            src.id,
            VideoSettings { res: 1080, fps: 60, bitrate: 12000, cursor: true },
            Callbacks {
                on_packet: Arc::new(move |_, _| { p.fetch_add(1, Ordering::SeqCst); }),
                on_info: Arc::new(move |i| {
                    println!("  {} hw={} zero-copy={} {}x{} {:.1} fps {} kb/s (alvo {}) encode {:.1} ms, capturados {:.0}/s", i.encoder, i.hardware, i.zero_copy, i.width, i.height, i.fps, i.kbps, i.target_kbps, i.encode_ms, i.captured_fps);
                    inf.lock().unwrap().push(i);
                }),
                on_preview: Arc::new(|_| {}),
                on_error: Arc::new(|e| println!("  ERRO {e}")),
                on_source_closed: Arc::new(|| println!("  fonte fechou")),
            },
        )
        .expect("pipeline");
        std::thread::sleep(Duration::from_secs(4));
        pipe.stop();
        let infos = infos.lock().unwrap();
        let last = infos.last().expect("sem estatísticas");
        assert!(last.fps >= 55.0, "FPS abaixo do alvo: {:.1}", last.fps);
    }
}
