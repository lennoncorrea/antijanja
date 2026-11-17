//! Captura de áudio do sistema com controle por aplicativo (Windows 10 2004+).
//!
//! Usa o "process loopback" do WASAPI, que captura o áudio de uma árvore de processos
//! ou de tudo, *exceto* uma árvore de processos. Isso permite, por exemplo, transmitir
//! o áudio do PC sem o áudio do Discord.
//!
//! O áudio é capturado em float 48 kHz estéreo, convertido para PCM 16 bits e enviado
//! ao frontend em blocos de 20 ms por um canal binário do Tauri. No frontend, um
//! AudioWorklet transforma isso numa faixa de áudio WebRTC.

use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
use wasapi::{
    initialize_mta, AudioClient, DeviceEnumerator, Direction, SampleType, SessionState, StreamMode,
    WaveFormat,
};

const CHUNK_FRAMES: usize = 960; // 20 ms a 48 kHz
const BYTES_PER_FRAME_F32: usize = 8; // 2 canais × 4 bytes
const RECHECK_EVERY: Duration = Duration::from_secs(3);


#[derive(Serialize, Clone)]
pub struct AudioApp {
    /// Nome amigável (ex.: "Discord").
    pub name: String,
    /// Executável em minúsculas (ex.: "discord.exe"), usado como identificador.
    pub exe: String,
    /// Está tocando áudio agora?
    pub active: bool,
}

#[derive(Clone)]
enum Mode {
    /// Todo o áudio do PC.
    System,
    /// Todo o áudio, exceto o deste executável.
    Exclude(String),
    /// Somente o áudio deste executável.
    Include(String),
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn snapshot() -> Result<System, String> {
    System::new_with_specifics(RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing()))
        .map_err(|e| format!("Não foi possível listar os processos: {e}"))
}

fn exe_of(sys: &System, pid: Pid) -> Option<String> {
    sys.process(pid).map(|p| p.name().to_string_lossy().to_lowercase())
}

/// Sobe na árvore enquanto o pai for o mesmo executável (ex.: Discord.exe → Discord.exe).
/// O process loopback precisa do processo raiz para pegar a árvore inteira.
fn root_of(sys: &System, pid: Pid) -> Pid {
    let Some(name) = exe_of(sys, pid) else { return pid };
    let mut cur = pid;
    for _ in 0..32 {
        match sys.process(cur).and_then(|p| p.parent()) {
            Some(parent) if exe_of(sys, parent).as_deref() == Some(name.as_str()) => cur = parent,
            _ => break,
        }
    }
    cur
}

fn find_root_by_exe(sys: &System, exe: &str) -> Option<u32> {
    sys.processes()
        .iter()
        .find(|(_, p)| p.name().to_string_lossy().eq_ignore_ascii_case(exe))
        .map(|(pid, _)| root_of(sys, *pid).as_u32())
}

fn pretty_name(exe: &str) -> String {
    let known = [
        ("discord.exe", "Discord"),
        ("discordptb.exe", "Discord PTB"),
        ("discordcanary.exe", "Discord Canary"),
        ("chrome.exe", "Google Chrome"),
        ("msedge.exe", "Microsoft Edge"),
        ("firefox.exe", "Firefox"),
        ("opera.exe", "Opera"),
        ("brave.exe", "Brave"),
        ("spotify.exe", "Spotify"),
        ("steam.exe", "Steam"),
        ("vlc.exe", "VLC"),
        ("ms-teams.exe", "Microsoft Teams"),
        ("zoom.exe", "Zoom"),
        ("whatsapp.exe", "WhatsApp"),
        ("telegram.exe", "Telegram"),
    ];
    if let Some((_, name)) = known.iter().find(|(e, _)| *e == exe) {
        return name.to_string();
    }
    let stem = exe.strip_suffix(".exe").unwrap_or(exe);
    let mut chars = stem.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => exe.to_string(),
    }
}

/// Lista os apps com sessão de áudio (tocando ou não) + o Discord, se estiver aberto.
pub fn audio_apps() -> Result<Vec<AudioApp>, String> {
    thread::spawn(|| -> Result<Vec<AudioApp>, String> {
        let _ = initialize_mta();
        let sys = snapshot()?;
        let own_exe = exe_of(&sys, Pid::from_u32(std::process::id())).unwrap_or_default();
        let mut apps: BTreeMap<String, AudioApp> = BTreeMap::new();

        let enumerator = DeviceEnumerator::new().map_err(err)?;
        let devices = enumerator.get_device_collection(Direction::Render).map_err(err)?;
        for device in &devices {
            let Ok(device) = device else { continue };
            let Ok(sessions) = device
                .get_iaudiosessionmanager()
                .and_then(|m| m.get_audiosessionenumerator())
            else {
                continue;
            };
            for i in 0..sessions.get_count().unwrap_or(0) {
                let Ok(control) = sessions.get_session(i) else { continue };
                let Ok(pid) = control.get_process_id() else { continue };
                if pid == 0 {
                    continue; // sons do sistema
                }
                let root = root_of(&sys, Pid::from_u32(pid));
                let Some(exe) = exe_of(&sys, root) else { continue };
                if exe == own_exe {
                    continue;
                }
                let active = matches!(control.get_state(), Ok(SessionState::Active));
                apps.entry(exe.clone())
                    .and_modify(|a| a.active |= active)
                    .or_insert(AudioApp { name: pretty_name(&exe), exe, active });
            }
        }

        // O Discord pode estar aberto sem sessão de áudio no momento.
        for exe in ["discord.exe", "discordptb.exe", "discordcanary.exe"] {
            if find_root_by_exe(&sys, exe).is_some() {
                apps.entry(exe.to_string()).or_insert(AudioApp {
                    name: pretty_name(exe),
                    exe: exe.to_string(),
                    active: false,
                });
            }
        }

        let mut list: Vec<AudioApp> = apps.into_values().collect();
        list.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| a.name.cmp(&b.name)));
        Ok(list)
    })
    .join()
    .map_err(|_| "falha ao listar apps".to_string())?
}

/// Status da captura: ("capturing" | "waiting" | "error", detalhe).
pub type StatusFn = Arc<dyn Fn(&str, &str) + Send + Sync>;
/// Recebe blocos de 20 ms em PCM 16 bits estéreo (48 kHz) e o instante em que a primeira
/// amostra foi capturada (relógio de alta precisão do Windows; None se ele não informou).
/// Devolve `false` para parar.
pub type PcmSink = Box<dyn FnMut(Vec<u8>, Option<Instant>) -> bool + Send>;

/// Relógio de alta precisão do Windows em unidades de 100 ns (o que o WASAPI usa nos
/// horários de captura).
fn qpc_100ns() -> u64 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut c, mut f) = (0i64, 0i64);
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
        let _ = QueryPerformanceFrequency(&mut f);
    }
    if f <= 0 {
        return 0;
    }
    (c as i128 * 10_000_000 / f as i128) as u64
}

/// Converte um horário do WASAPI (100 ns do relógio de alta precisão) em `Instant`.
fn instant_from_qpc(t: u64) -> Instant {
    let (now_i, now_q) = (Instant::now(), qpc_100ns());
    if t >= now_q || now_q == 0 {
        return now_i;
    }
    now_i.checked_sub(Duration::from_nanos((now_q - t).saturating_mul(100))).unwrap_or(now_i)
}

/// Captura de áudio em andamento; para ao ser descartada.
pub struct AudioCapture {
    stop: Arc<AtomicBool>,
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Começa a capturar. `mode`: "system" | "exclude" | "include".
pub fn start_capture(mode: &str, app: Option<String>, sink: PcmSink, status: StatusFn) -> Result<AudioCapture, String> {
    let app = app.map(|a| a.to_lowercase()).filter(|a| !a.is_empty());
    let mode = match (mode, app) {
        ("system", _) => Mode::System,
        ("exclude", Some(exe)) => Mode::Exclude(exe),
        ("include", Some(exe)) => Mode::Include(exe),
        _ => return Err("Escolha um aplicativo.".into()),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    thread::Builder::new()
        .name("audio-capture".into())
        .spawn(move || capture_thread(mode, flag, sink, status))
        .map_err(err)?;
    Ok(AudioCapture { stop })
}

/// Resolve o processo-alvo atual: (pid, incluir_árvore). `None` = app não está aberto.
fn resolve(mode: &Mode, sys: &System) -> Option<(u32, bool)> {
    let me = std::process::id();
    match mode {
        Mode::System => Some((me, false)), // tudo, menos o próprio AntiJanja
        Mode::Exclude(exe) => Some((find_root_by_exe(sys, exe).unwrap_or(me), false)),
        Mode::Include(exe) => find_root_by_exe(sys, exe).map(|pid| (pid, true)),
    }
}

fn capture_thread(mode: Mode, stop: Arc<AtomicBool>, mut sink: PcmSink, status: StatusFn) {
    let _ = initialize_mta();
    let mut sys = match snapshot() {
        Ok(sys) => sys,
        Err(e) => return status("error", &e),
    };
    let mut failures = 0;

    while !stop.load(Ordering::SeqCst) {
        sys.refresh_processes(ProcessesToUpdate::All, true);
        let Some(target) = resolve(&mode, &sys) else {
            if let Mode::Include(exe) = &mode {
                status("waiting", &pretty_name(exe));
            }
            sleep_unless(&stop, RECHECK_EVERY);
            continue;
        };

        let detail = match &mode {
            Mode::Exclude(exe) if target.0 == std::process::id() => format!("{} não está aberto", pretty_name(exe)),
            _ => String::new(),
        };
        status("capturing", &detail);

        match run_capture(target, &mode, &mut sys, &stop, &mut sink) {
            Ok(()) => failures = 0, // alvo mudou ou foi parado: reinicia/sai
            Err(e) => {
                failures += 1;
                status("error", &e);
                if failures >= 3 {
                    return;
                }
                sleep_unless(&stop, Duration::from_secs(2));
            }
        }
    }
}

/* ========================================================================== */
/*                         Opus: codificação e reprodução                      */
/* ========================================================================== */

/// Codifica blocos de 20 ms (960 quadros estéreo) em Opus.
pub struct OpusEncoder {
    enc: opus::Encoder,
    out: Vec<u8>,
    gain: f32,
}

impl OpusEncoder {
    pub fn new(kbps: u32) -> Result<Self, String> {
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio).map_err(err)?;
        enc.set_bitrate(opus::Bitrate::Bits((kbps * 1000) as i32)).map_err(err)?;
        enc.set_inband_fec(true).ok();
        enc.set_packet_loss_perc(5).ok();
        Ok(Self { enc, out: vec![0u8; 4000], gain: 1.0 })
    }

    pub fn set_bitrate(&mut self, kbps: u32) {
        let _ = self.enc.set_bitrate(opus::Bitrate::Bits((kbps * 1000) as i32));
    }

    /// Volume aplicado antes de codificar (1.0 = 100%).
    pub fn set_gain(&mut self, gain: f32) {
        self.gain = gain.clamp(0.0, 4.0);
    }

    /// `pcm`: 960 quadros estéreo em i16 little-endian (3840 bytes).
    pub fn encode(&mut self, pcm: &[u8]) -> Result<Vec<u8>, String> {
        let samples: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|b| {
                let s = i16::from_le_bytes([b[0], b[1]]) as f32 * self.gain;
                s.clamp(-32768.0, 32767.0) as i16
            })
            .collect();
        let n = self.enc.encode(&samples, &mut self.out).map_err(err)?;
        Ok(self.out[..n].to_vec())
    }
}

/// Toca o áudio recebido (Opus) no dispositivo padrão, com volume ajustável.
pub struct Playback {
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    volume: Arc<std::sync::atomic::AtomicU32>,
    stop: Arc<AtomicBool>,
    /// Modo sincronizado (áudio em pedaços): o relógio comum e o áudio na linha do tempo.
    sync: SyncSlot,
}

type SyncSlot = Arc<std::sync::Mutex<Option<(Arc<crate::avsync::SyncClock>, Arc<std::sync::Mutex<crate::avsync::AudioState>>)>>>;

impl Playback {
    pub fn start() -> Result<Self, String> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(50);
        let volume = Arc::new(std::sync::atomic::AtomicU32::new(1.0f32.to_bits()));
        let stop = Arc::new(AtomicBool::new(false));
        let sync: SyncSlot = Arc::new(std::sync::Mutex::new(None));
        let (v, s, y) = (volume.clone(), stop.clone(), sync.clone());
        thread::Builder::new()
            .name("audio-playback".into())
            .spawn(move || {
                if let Err(e) = playback_thread(rx, v, s, y) {
                    eprintln!("reprodução de áudio: {e}");
                }
            })
            .map_err(err)?;
        Ok(Self { tx, volume, stop, sync })
    }

    /// Passa a tocar pelo relógio comum (áudio em pedaços, sincronizado com o vídeo).
    pub fn attach_sync(&self, clock: Arc<crate::avsync::SyncClock>, state: Arc<std::sync::Mutex<crate::avsync::AudioState>>) {
        *self.sync.lock().unwrap() = Some((clock, state));
    }

    /// 0.0 a 1.0
    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// PCM float intercalado estéreo, 48 kHz.
    pub fn push(&self, pcm: Vec<f32>) {
        let _ = self.tx.try_send(pcm);
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Decodificador Opus do espectador.
pub struct OpusDecoder {
    dec: opus::Decoder,
    buf: Vec<i16>,
}

impl OpusDecoder {
    pub fn new() -> Result<Self, String> {
        Ok(Self { dec: opus::Decoder::new(48_000, opus::Channels::Stereo).map_err(err)?, buf: vec![0i16; 5760 * 2] })
    }

    pub fn decode(&mut self, packet: &[u8]) -> Vec<f32> {
        match self.dec.decode(packet, &mut self.buf, false) {
            Ok(n) => self.buf[..n * 2].iter().map(|&s| s as f32 / 32768.0).collect(),
            Err(_) => Vec::new(),
        }
    }
}

fn playback_thread(
    rx: std::sync::mpsc::Receiver<Vec<f32>>,
    volume: Arc<std::sync::atomic::AtomicU32>,
    stop: Arc<AtomicBool>,
    sync: SyncSlot,
) -> Result<(), String> {
    let _ = initialize_mta();
    let device = DeviceEnumerator::new().map_err(err)?.get_default_device(Direction::Render).map_err(err)?;
    let mut client = device.get_iaudioclient().map_err(err)?;
    let format = WaveFormat::new(32, 32, SampleType::Float, 48_000, 2, None);
    let (period, _) = client.get_device_period().map_err(err)?;
    client
        .initialize_client(&format, Direction::Render, StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: period }, None)
        .map_err(err)?;
    let event = client.set_get_eventhandle().map_err(err)?;
    let render = client.get_audiorenderclient().map_err(err)?;
    // Latência do dispositivo depois do buffer (100 ns): entra no horário em que cada bloco
    // sai de fato no alto-falante.
    let stream_latency = Duration::from_nanos(client.get_stream_latency().unwrap_or(0).max(0) as u64 * 100);
    client.start_stream().map_err(err)?;

    // Buffer de jitter: começa a tocar com ~60 ms; acima de ~200 ms descarta o excesso.
    const START: usize = 48 * 60 * 2;
    const MAX: usize = 48 * 200 * 2;
    const TRIM: usize = 48 * 80 * 2;
    let mut queue: VecDeque<f32> = VecDeque::with_capacity(MAX * 2);
    let mut playing = false;

    while !stop.load(Ordering::SeqCst) {
        let synced = sync.lock().unwrap().clone();
        if let Some((clock, state)) = synced {
            // Modo sincronizado: cada bloco sai do ponto da linha do tempo que deve estar
            // tocando no instante em que ele vai sair no alto-falante.
            while rx.try_recv().is_ok() {}
            if event.wait_for_event(100).is_err() {
                continue;
            }
            let frames = client.get_available_space_in_frames().map_err(err)? as usize;
            if frames == 0 {
                continue;
            }
            let padding = client.get_current_padding().unwrap_or(0) as f64;
            let now = Instant::now();
            let heard_at = now + Duration::from_secs_f64(padding / 48_000.0) + stream_latency;
            let pcm = crate::avsync::render_block(&clock, &mut state.lock().unwrap(), now, heard_at, frames);
            let vol = f32::from_bits(volume.load(Ordering::Relaxed));
            let mut out = Vec::with_capacity(frames * 8);
            for s in pcm {
                out.extend_from_slice(&(s * vol).to_le_bytes());
            }
            render.write_to_device(&out, None).map_err(err)?;
            continue;
        }
        while let Ok(chunk) = rx.try_recv() {
            queue.extend(chunk);
        }
        if queue.len() > MAX {
            let drop_n = queue.len() - TRIM;
            queue.drain(..drop_n);
        }
        if !playing && queue.len() >= START {
            playing = true;
        }
        if event.wait_for_event(100).is_err() {
            continue;
        }
        let frames = client.get_available_space_in_frames().map_err(err)? as usize;
        if frames == 0 {
            continue;
        }
        let vol = f32::from_bits(volume.load(Ordering::Relaxed));
        let mut out = Vec::with_capacity(frames * 8);
        for _ in 0..frames * 2 {
            let s = if playing { queue.pop_front() } else { None };
            out.extend_from_slice(&(s.unwrap_or(0.0) * vol).to_le_bytes());
        }
        if playing && queue.is_empty() {
            playing = false; // faltou dado: espera acumular de novo
        }
        render.write_to_device(&out, None).map_err(err)?;
    }
    let _ = client.stop_stream();
    Ok(())
}

fn sleep_unless(stop: &AtomicBool, total: Duration) {
    let until = Instant::now() + total;
    while Instant::now() < until && !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(100));
    }
}

/// Entrega os blocos completos de 20 ms da fila (f32 → i16), cada um com o horário de
/// captura dele. `false` = quem recebe pediu para parar.
fn emit_chunks(
    queue: &mut VecDeque<u8>,
    queue_t: &mut Option<u64>,
    chunk_bytes: usize,
    chunk_t: u64,
    sink: &mut dyn FnMut(Vec<u8>, Option<Instant>) -> bool,
) -> bool {
    while queue.len() >= chunk_bytes {
        let mut out = Vec::with_capacity(CHUNK_FRAMES * 4);
        let raw: Vec<u8> = queue.drain(..chunk_bytes).collect();
        for sample in raw.chunks_exact(4) {
            let f = f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]);
            let s = (f.clamp(-1.0, 1.0) * 32767.0) as i16;
            out.extend_from_slice(&s.to_le_bytes());
        }
        let at = queue_t.map(instant_from_qpc);
        if let Some(t) = queue_t.as_mut() {
            *t += chunk_t;
        }
        if !sink(out, at) {
            return false;
        }
    }
    true
}

/// Captura até ser parado ou até o processo-alvo mudar (ex.: Discord abriu/fechou).
fn run_capture(
    (pid, include): (u32, bool),
    mode: &Mode,
    sys: &mut System,
    stop: &AtomicBool,
    sink: &mut dyn FnMut(Vec<u8>, Option<Instant>) -> bool,
) -> Result<(), String> {
    let format = WaveFormat::new(32, 32, SampleType::Float, 48_000, 2, None);
    let mut client = AudioClient::new_application_loopback_client(pid, include)
        .map_err(|e| format!("Captura de áudio por app indisponível (requer Windows 10 2004+): {e}"))?;
    client
        .initialize_client(
            &format,
            Direction::Capture,
            StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 },
            None,
        )
        .map_err(err)?;
    let event = client.set_get_eventhandle().map_err(err)?;
    let capture = client.get_audiocaptureclient().map_err(err)?;
    client.start_stream().map_err(err)?;

    let chunk_bytes = CHUNK_FRAMES * BYTES_PER_FRAME_F32;
    let mut queue: VecDeque<u8> = VecDeque::with_capacity(chunk_bytes * 8);
    let mut last_check = Instant::now();
    // Horário de captura (100 ns do relógio de alta precisão) da primeira amostra na fila.
    let mut queue_t: Option<u64> = None;
    // 20 ms em unidades de 100 ns.
    let chunk_t = (CHUNK_FRAMES as u64) * 10_000_000 / 48_000;

    let result = loop {
        if stop.load(Ordering::SeqCst) {
            break Ok(());
        }
        if last_check.elapsed() >= RECHECK_EVERY {
            last_check = Instant::now();
            sys.refresh_processes(ProcessesToUpdate::All, true);
            if resolve(mode, sys) != Some((pid, include)) {
                break Ok(()); // alvo mudou: reinicia com o novo
            }
        }
        // Sem áudio tocando, o evento não dispara; o frontend preenche com silêncio.
        if event.wait_for_event(200).is_err() {
            continue;
        }
        loop {
            match capture.get_next_packet_size() {
                Ok(Some(n)) if n > 0 => {}
                Ok(_) => break,
                Err(e) => return Err(e.to_string()),
            }
            let before = queue.len();
            let info = capture.read_from_device_to_deque(&mut queue).map_err(err)?;
            if info.flags.silent {
                queue.range_mut(before..).for_each(|b| *b = 0);
            }
            // Horário da captura: o do primeiro quadro deste pacote. Se ele não emenda com o
            // que já estava na fila (o áudio parou e voltou), o resto antigo sai completado
            // com silêncio e a fila recomeça no horário novo (cada bloco leva o horário certo).
            let ts = (info.timestamp > 0).then_some(info.timestamp);
            let before_frames = (before / BYTES_PER_FRAME_F32) as u64;
            let expected = queue_t.map(|t| t + before_frames * 10_000_000 / 48_000);
            let continuous = match (expected, ts) {
                (Some(e), Some(t)) => (t as i64 - e as i64).abs() < 50_000, // 5 ms
                _ => true,
            };
            if before == 0 {
                queue_t = ts;
            } else if !continuous {
                let new: Vec<u8> = queue.drain(before..).collect();
                let pad = (chunk_bytes - before % chunk_bytes) % chunk_bytes;
                queue.extend(std::iter::repeat_n(0u8, pad));
                if !emit_chunks(&mut queue, &mut queue_t, chunk_bytes, chunk_t, sink) {
                    stop.store(true, Ordering::SeqCst);
                    break;
                }
                queue.extend(new);
                queue_t = ts;
            }
        }
        if !emit_chunks(&mut queue, &mut queue_t, chunk_bytes, chunk_t, sink) {
            stop.store(true, Ordering::SeqCst); // a janela fechou
        }
    };
    let _ = client.stop_stream();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O tocador de verdade (WASAPI) no modo sincronizado: toca um tom contínuo vindo de
    /// pacotes Opus (como chegam pela malha, com o relógio comum) e, ao mesmo tempo, captura
    /// o próprio áudio que está saindo (captura por aplicativo do Windows). O que saiu não
    /// pode ter buraco. Toca baixinho (2%) por ~5 s.
    /// `cargo test synced_playback_on_the_real_device -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn synced_playback_on_the_real_device() {
        use crate::avsync::{AudioState, Stream, SyncClock, MEDIA_HZ, PACKET_SAMPLES};
        let _ = initialize_mta();
        let t0 = Instant::now();
        let clock = Arc::new(SyncClock::new(t0));
        let state = Arc::new(std::sync::Mutex::new(AudioState::new().unwrap()));
        let pb = Playback::start().unwrap();
        pb.set_volume(0.02);
        pb.attach_sync(clock.clone(), state.clone());

        // Captura o áudio deste processo enquanto toca.
        let captured = Arc::new(std::sync::Mutex::new(Vec::<i16>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let cap = {
            let (captured, stop) = (captured.clone(), stop.clone());
            thread::spawn(move || {
                let _ = initialize_mta();
                let exe = std::env::current_exe().unwrap().file_name().unwrap().to_string_lossy().into_owned();
                let mut sys = snapshot().unwrap();
                let mut sink = |pcm: Vec<u8>, _: Option<Instant>| {
                    let mut c = captured.lock().unwrap();
                    c.extend(pcm.chunks_exact(4).map(|b| i16::from_le_bytes([b[0], b[1]])));
                    true
                };
                // A captura confere o alvo a cada 3 s e pode recomeçar: repete até o fim.
                while !stop.load(Ordering::SeqCst) {
                    run_capture((std::process::id(), true), &Mode::Include(exe.clone()), &mut sys, &stop, &mut sink).expect("captura");
                }
            })
        };

        // Pacotes de 20 ms de um tom de 440 Hz, chegando em tempo real (15 ms depois da
        // "captura", com variação de até 8 ms).
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        let mut out = vec![0u8; 4000];
        let mut rng = 3u32;
        for k in 0..250u32 {
            let cap_ms = k as f64 * 20.0;
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            let arrive = t0 + Duration::from_secs_f64((cap_ms + 15.0 + (rng % 8) as f64) / 1000.0);
            if let Some(w) = arrive.checked_duration_since(Instant::now()) {
                thread::sleep(w);
            }
            let pcm: Vec<i16> = (0..PACKET_SAMPLES)
                .flat_map(|i| {
                    let t = (k as usize * PACKET_SAMPLES + i) as f32 / 48_000.0;
                    let v = ((t * 440.0 * std::f32::consts::TAU).sin() * 20_000.0) as i16;
                    [v, v]
                })
                .collect();
            let n = enc.encode(&pcm, &mut out).unwrap();
            let ts = (cap_ms * MEDIA_HZ / 1000.0) as u32;
            clock.observe(Stream::Audio, ts, Instant::now());
            let mut st = state.lock().unwrap();
            let AudioState { rx, tl } = &mut *st;
            rx.push(k, ts, out[..n].to_vec());
            rx.run(&clock, Instant::now(), tl);
        }
        thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::SeqCst);
        cap.join().unwrap();
        drop(pb);

        let c = captured.lock().unwrap();
        let left: Vec<i16> = c.iter().step_by(2).copied().collect();
        let first = left.iter().position(|v| v.unsigned_abs() > 50).expect("não saiu som nenhum");
        let last = left.iter().rposition(|v| v.unsigned_abs() > 50).unwrap();
        // Buracos: trechos de 2 ms seguidos sem som entre o começo e o fim do tom.
        let mut gaps = 0;
        let mut quiet = 0;
        for v in &left[first..last] {
            if v.unsigned_abs() < 8 {
                quiet += 1;
                if quiet == 96 {
                    gaps += 1;
                }
            } else {
                quiet = 0;
            }
        }
        let st = state.lock().unwrap();
        println!("captura: {:.2} s no total, tom de {:.2} s a {:.2} s", left.len() as f64 / 48_000.0, first as f64 / 48_000.0, last as f64 / 48_000.0);
        println!(
            "som capturado: {:.2} s de tom, buracos {gaps}; FEC {}, PLC {}, pulos {}, faltando {}",
            (last - first) as f64 / 48_000.0, st.rx.fec, st.rx.plc, st.tl.snaps, st.tl.missing
        );
        // A captura por aplicativo para de entregar quando é reiniciada (ela confere o alvo a
        // cada 3 s): a verificação cobre a janela da primeira captura (~2,3 s de tom).
        assert!((last - first) as f64 / 48_000.0 > 2.0, "tocou pouco");
        assert_eq!(gaps, 0, "buraco no som que saiu");
        assert_eq!(st.tl.snaps, 0);
    }

    /// Precisa de um dispositivo de áudio: `cargo test audio -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_capture_and_listing() {
        let apps = audio_apps().expect("listar apps");
        println!("apps: {:?}", apps.iter().map(|a| format!("{} ({}){}", a.name, a.exe, if a.active { " *" } else { "" })).collect::<Vec<_>>());

        let _ = initialize_mta();
        let mut sys = snapshot().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopper = stop.clone();
        thread::spawn(move || { thread::sleep(Duration::from_secs(4)); stopper.store(true, Ordering::SeqCst); });
        let mut chunks = 0usize;
        let mut sink = |pcm: Vec<u8>, _: Option<Instant>| { assert_eq!(pcm.len(), CHUNK_FRAMES * 4); chunks += 1; true };
        let target = resolve(&Mode::System, &sys).unwrap();
        run_capture(target, &Mode::System, &mut sys, &stop, &mut sink).expect("captura");
        println!("blocos de 20 ms capturados em 4 s: {chunks} (0 = nada tocando)");
    }
}
