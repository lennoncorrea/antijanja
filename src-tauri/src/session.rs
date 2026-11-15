//! Sessões de transmissão (host) e de assistir (espectador), expostas como comandos Tauri.

use crate::audio::{self, AudioCapture, OpusDecoder, OpusEncoder, Playback};
use crate::capture::{self, Source};
use crate::media::{mf_init, Decoder};
use crate::player::{Player, PlayerFeed};
use crate::rendezvous::{self, RdvState};
use crate::transport::{self, HostPeer, PeerEvent, ViewerSession, ViewerSinks};
use crate::video::{Callbacks, VideoPipeline, VideoSettings};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

type Res<T> = Result<T, String>;

const JOIN_TIMEOUT_S: u64 = 90;

/* ========================================================================== */
/*                                    HOST                                      */
/* ========================================================================== */

#[derive(Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HostConfig {
    pub source: String,
    pub name: String,
    pub res: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub cursor: bool,
    pub audio_mode: String,
    pub audio_app: String,
    pub abitrate: u32,
    pub volume: u32,
    /// Aparecer na lista pública de transmissões.
    #[serde(default)]
    pub public: bool,
    /// Nome da tela/janela transmitida (aparece na lista pública).
    #[serde(default)]
    pub title: String,
}

impl HostConfig {
    fn quality(&self) -> String {
        if self.res == 0 { format!("original {}fps", self.fps) } else { format!("{}p{}", self.res, self.fps) }
    }

    fn video(&self) -> VideoSettings {
        VideoSettings { res: self.res, fps: self.fps.clamp(5, 144), bitrate: self.bitrate, cursor: self.cursor }
    }
}

#[derive(Serialize, Clone)]
struct PeerInfo {
    id: u64,
    name: String,
    state: &'static str,
}

struct PeerEntry {
    name: String,
    state: &'static str,
    peer: Arc<HostPeer>,
}

enum Media {
    Video(Bytes, Duration),
    Audio(Bytes),
}

struct HostSession {
    code: String,
    config: HostConfig,
    video: VideoPipeline,
    audio: Option<AudioCapture>,
    opus: Arc<Mutex<OpusEncoder>>,
    peers: Arc<Mutex<HashMap<u64, PeerEntry>>>,
    media_tx: tokio::sync::mpsc::Sender<Media>,
    on_offer: Arc<dyn Fn(u8, String) + Send + Sync>,
    public: Option<rendezvous::PublicAnnouncer>,
    since: i64,
}

fn start_public(state: &RdvState, cfg: &HostConfig, code: &str, since: i64, viewers: u32) -> Option<rendezvous::PublicAnnouncer> {
    if !cfg.public {
        return None;
    }
    let entry = rendezvous::PublicEntry {
        id: rendezvous::instance_id().to_string(),
        name: cfg.name.chars().take(40).collect(),
        title: cfg.title.chars().take(80).collect(),
        code: code.to_string(),
        viewers,
        quality: cfg.quality(),
        since,
        ts: rendezvous::now_ms(),
        gone: false,
    };
    rendezvous::PublicAnnouncer::start(state, entry).ok()
}

static HOST: Mutex<Option<HostSession>> = Mutex::new(None);

/// Ofertas já vistas, por `sid`: `None` enquanto a resposta é montada, depois (slot, resposta).
type Offers = Arc<Mutex<HashMap<String, Option<(u8, String)>>>>;
static PEER_SEQ: AtomicU64 = AtomicU64::new(0);

fn emit_peers(app: &AppHandle, peers: &Mutex<HashMap<u64, PeerEntry>>) {
    let mut list: Vec<PeerInfo> = peers
        .lock()
        .unwrap()
        .iter()
        .map(|(id, p)| PeerInfo { id: *id, name: p.name.clone(), state: p.state })
        .collect();
    list.sort_by_key(|p| p.id);
    let watching = list.iter().filter(|p| p.state != "connecting").count() as u32;
    if let Some(h) = HOST.lock().unwrap().as_ref() {
        if let Some(a) = &h.public {
            a.update(|e| e.viewers = watching, false);
        }
    }
    let _ = app.emit("host-peers", list);
}

fn start_audio(app: &AppHandle, cfg: &HostConfig, opus: Arc<Mutex<OpusEncoder>>, tx: tokio::sync::mpsc::Sender<Media>) -> Option<AudioCapture> {
    if cfg.audio_mode == "none" || cfg.audio_mode.is_empty() {
        let _ = app.emit("host-audio", serde_json::json!({ "status": "off", "detail": "" }));
        return None;
    }
    let app2 = app.clone();
    let mut last_level = Instant::now();
    let app_level = app.clone();
    let sink: audio::PcmSink = Box::new(move |pcm: Vec<u8>| {
        if last_level.elapsed() > Duration::from_millis(100) {
            last_level = Instant::now();
            let peak = pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs()).max().unwrap_or(0);
            let _ = app_level.emit("host-audio-level", (peak as f32 / 32768.0).sqrt());
        }
        if let Ok(pkt) = opus.lock().unwrap().encode(&pcm) {
            let _ = tx.try_send(Media::Audio(Bytes::from(pkt)));
        }
        true
    });
    let status: audio::StatusFn = Arc::new(move |s: &str, d: &str| {
        let _ = app2.emit("host-audio", serde_json::json!({ "status": s, "detail": d }));
    });
    match audio::start_capture(&cfg.audio_mode, Some(cfg.audio_app.clone()), sink, status) {
        Ok(c) => Some(c),
        Err(e) => {
            let _ = app.emit("host-audio", serde_json::json!({ "status": "error", "detail": e }));
            None
        }
    }
}

/// Lista telas e janelas com miniaturas.
#[tauri::command(async)]
pub fn sources_list() -> Vec<Source> {
    capture::list_sources()
}

/// Começa a transmitir e devolve o código.
#[tauri::command(async)]
pub fn host_start(app: AppHandle, state: State<'_, RdvState>, config: HostConfig) -> Res<String> {
    stop_host(&state);
    let (media_tx, mut media_rx) = tokio::sync::mpsc::channel::<Media>(240);
    let peers: Arc<Mutex<HashMap<u64, PeerEntry>>> = Arc::new(Mutex::new(HashMap::new()));

    // Distribui os pacotes para todos os espectadores conectados.
    {
        let peers = peers.clone();
        transport::spawn(async move {
            while let Some(m) = media_rx.recv().await {
                let targets: Vec<Arc<HostPeer>> = peers
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|p| p.state != "connecting")
                    .map(|p| p.peer.clone())
                    .collect();
                for p in targets {
                    let _ = match &m {
                        Media::Video(d, dur) => p.send_video(d.clone(), *dur).await,
                        Media::Audio(d) => p.send_audio(d.clone(), Duration::from_millis(20)).await,
                    };
                }
            }
        });
    }

    // Pipeline de vídeo.
    let keyframe_needed = Arc::new(AtomicUsize::new(0));
    let video = {
        let (a1, a2, a3, a4) = (app.clone(), app.clone(), app.clone(), app.clone());
        let tx = media_tx.clone();
        let kf = keyframe_needed.clone();
        VideoPipeline::start(
            config.source.clone(),
            config.video(),
            Callbacks {
                on_packet: Arc::new(move |data, dur| {
                    if tx.try_send(Media::Video(Bytes::from(data), dur)).is_err() {
                        kf.fetch_add(1, Ordering::SeqCst); // fila cheia: quadro perdido
                    }
                }),
                on_info: Arc::new(move |i| { let _ = a1.emit("host-info", i); }),
                on_preview: Arc::new(move |j| { let _ = a2.emit("host-preview", j); }),
                on_error: Arc::new(move |e| { let _ = a3.emit("host-error", e); }),
                on_source_closed: Arc::new(move || { let _ = a4.emit("host-source-closed", ()); }),
            },
        )?
    };

    let opus = Arc::new(Mutex::new(OpusEncoder::new(config.abitrate.clamp(32, 510))?));
    opus.lock().unwrap().set_gain(config.volume as f32 / 100.0);
    let audio = start_audio(&app, &config, opus.clone(), media_tx.clone());

    // Pedidos de conexão vindos da DHT.
    let on_offer: Arc<dyn Fn(u8, String) + Send + Sync> = {
        let app = app.clone();
        let peers = peers.clone();
        let video = video.clone();
        let offers: Offers = Arc::new(Mutex::new(HashMap::new()));
        Arc::new(move |slot: u8, payload: String| {
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(&payload) else { return };
            let (Some(sdp), Some(sid)) = (msg["sdp"].as_str(), msg["sid"].as_str()) else { return };
            if msg["t"] != "offer" {
                return;
            }
            let mut map = offers.lock().unwrap();
            match map.get(sid) {
                // Oferta repetida: o espectador não recebeu a resposta (a DHT perdeu ou
                // ainda não propagou). Publica a mesma resposta de novo.
                Some(Some((reply_slot, answer))) => {
                    let (reply_slot, answer, app) = (*reply_slot, answer.clone(), app.clone());
                    drop(map);
                    let Some(code) = HOST.lock().unwrap().as_ref().map(|h| h.code.clone()) else { return };
                    std::thread::spawn(move || {
                        let _ = rendezvous::host_reply(&app.state::<RdvState>(), &code, reply_slot, &answer);
                    });
                }
                Some(None) => {} // ainda montando a resposta
                None => {
                    map.insert(sid.to_string(), None);
                    drop(map);
                    let name: String = msg["name"].as_str().unwrap_or("Espectador").chars().take(40).collect();
                    let (app, peers, video, offers) = (app.clone(), peers.clone(), video.clone(), offers.clone());
                    let (sdp, sid) = (sdp.to_string(), sid.to_string());
                    transport::spawn(async move { accept_viewer(app, peers, video, offers, slot, sid, sdp, name).await });
                }
            }
        })
    };

    let code = rendezvous::host_listen(&state, on_offer.clone())?;
    let since = rendezvous::now_ms();
    let public = start_public(&state, &config, &code, since, 0);
    *HOST.lock().unwrap() = Some(HostSession { code: code.clone(), config, video, audio, opus, peers, media_tx, on_offer, public, since });

    // Se a fila de envio encher, pede keyframe para quem perdeu quadros se recuperar.
    transport::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let Some(video) = HOST.lock().unwrap().as_ref().map(|h| h.video.clone()) else { break };
            if keyframe_needed.swap(0, Ordering::SeqCst) > 0 {
                video.keyframe();
            }
        }
    });
    Ok(code)
}

async fn accept_viewer(
    app: AppHandle,
    peers: Arc<Mutex<HashMap<u64, PeerEntry>>>,
    video: VideoPipeline,
    offers: Offers,
    slot: u8,
    sid: String,
    sdp: String,
    name: String,
) {
    let id = PEER_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let on_state: Arc<dyn Fn(PeerEvent) + Send + Sync> = {
        let (app, peers, video, name) = (app.clone(), peers.clone(), video.clone(), name.clone());
        Arc::new(move |ev: PeerEvent| {
            let mut changed = true;
            {
                let mut map = peers.lock().unwrap();
                match ev {
                    PeerEvent::Connected => {
                        if let Some(p) = map.get_mut(&id) {
                            if p.state == "connecting" {
                                let _ = app.emit("host-viewer-joined", name.clone());
                            }
                            p.state = "connected";
                        }
                        video.keyframe();
                    }
                    PeerEvent::Disconnected => {
                        if let Some(p) = map.get_mut(&id) {
                            p.state = "unstable";
                        }
                    }
                    PeerEvent::Failed | PeerEvent::Closed => {
                        changed = map.remove(&id).is_some();
                        if changed {
                            let _ = app.emit("host-viewer-left", name.clone());
                        }
                    }
                }
            }
            if changed {
                emit_peers(&app, &peers);
            }
        })
    };
    let on_message: Arc<dyn Fn(String) + Send + Sync> = {
        let (video, peers, app) = (video.clone(), peers.clone(), app.clone());
        let last_kf = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(5)));
        Arc::new(move |m: String| {
            if m.contains("\"kf\"") {
                let mut last = last_kf.lock().unwrap();
                if last.elapsed() > Duration::from_millis(300) {
                    *last = Instant::now();
                    video.keyframe();
                }
            } else if m.contains("\"bye\"") {
                let removed = peers.lock().unwrap().remove(&id);
                if let Some(p) = removed {
                    let _ = app.emit("host-viewer-left", p.name.clone());
                    transport::spawn(async move { let _ = p.peer.pc.close().await; });
                    emit_peers(&app, &peers);
                }
            }
        })
    };

    let (peer, answer) = match HostPeer::answer(sdp, true, on_state, on_message).await {
        Ok(v) => v,
        Err(e) => {
            offers.lock().unwrap().remove(&sid); // deixa o espectador tentar de novo
            let _ = app.emit("host-error", format!("Falha ao conectar {name}: {e}"));
            return;
        }
    };
    peers.lock().unwrap().insert(id, PeerEntry { name: name.clone(), state: "connecting", peer: peer.clone() });
    emit_peers(&app, &peers);

    let Some(code) = HOST.lock().unwrap().as_ref().map(|h| h.code.clone()) else { return };
    let host_name = HOST.lock().unwrap().as_ref().map(|h| h.config.name.clone()).unwrap_or_default();
    let payload = serde_json::json!({ "t": "answer", "sid": sid, "sdp": answer, "name": host_name }).to_string();
    offers.lock().unwrap().insert(sid.clone(), Some((slot, payload.clone())));
    let app2 = app.clone();
    // Se a publicação falhar, o espectador republica a oferta e a resposta vai de novo.
    let _ = tokio::task::spawn_blocking(move || {
        rendezvous::host_reply(&app2.state::<RdvState>(), &code, slot, &payload)
    })
    .await;
    // A resposta pode demorar a chegar pela DHT; desiste se não conectar
    // (um pouco além do tempo máximo de espera do espectador).
    tokio::time::sleep(Duration::from_secs(JOIN_TIMEOUT_S + 30)).await;
    let stale = peers.lock().unwrap().get(&id).is_some_and(|p| p.state == "connecting");
    if stale {
        peers.lock().unwrap().remove(&id);
        emit_peers(&app, &peers);
        let _ = peer.pc.close().await;
    }
}

/// Aplica mudanças ao vivo (fonte, vídeo, áudio, nome).
#[tauri::command]
pub fn host_update(app: AppHandle, state: State<'_, RdvState>, config: HostConfig) -> Res<()> {
    let mut guard = HOST.lock().unwrap();
    let Some(h) = guard.as_mut() else { return Ok(()) };
    let old = h.config.clone();
    if config.source != old.source {
        h.video.set_source(config.source.clone());
    }
    if config.video() != old.video() {
        h.video.set_settings(config.video());
    }
    if config.abitrate != old.abitrate {
        h.opus.lock().unwrap().set_bitrate(config.abitrate.clamp(32, 510));
    }
    if config.volume != old.volume {
        h.opus.lock().unwrap().set_gain(config.volume as f32 / 100.0);
    }
    if config.audio_mode != old.audio_mode || config.audio_app != old.audio_app {
        h.audio = None;
        h.audio = start_audio(&app, &config, h.opus.clone(), h.media_tx.clone());
    }
    // Lista pública: liga/desliga e mantém os dados em dia.
    if config.public != old.public {
        let viewers = h.peers.lock().unwrap().values().filter(|p| p.state != "connecting").count() as u32;
        h.public = None;
        h.public = start_public(&state, &config, &h.code, h.since, viewers);
    } else if let Some(a) = &h.public {
        let changed = config.title != old.title || config.name != old.name || config.quality() != old.quality();
        let (name, title, quality): (String, String, String) =
            (config.name.chars().take(40).collect(), config.title.chars().take(80).collect(), config.quality());
        a.update(|e| { e.name = name; e.title = title; e.quality = quality; }, changed);
    }
    h.config = config;
    Ok(())
}

/// Gera outro código; quem já está assistindo continua.
#[tauri::command(async)]
pub fn host_new_code(state: State<'_, RdvState>) -> Res<String> {
    let on_offer = HOST.lock().unwrap().as_ref().map(|h| h.on_offer.clone()).ok_or("não está transmitindo")?;
    let code = rendezvous::host_listen(&state, on_offer)?;
    if let Some(h) = HOST.lock().unwrap().as_mut() {
        h.code = code.clone();
        if let Some(a) = &h.public {
            let c = code.clone();
            a.update(|e| e.code = c, true);
        }
    }
    Ok(code)
}

#[tauri::command]
pub fn host_kick(app: AppHandle, id: u64) {
    let Some(peers) = HOST.lock().unwrap().as_ref().map(|h| h.peers.clone()) else { return };
    let removed = peers.lock().unwrap().remove(&id);
    if let Some(p) = removed {
        transport::spawn(async move {
            p.peer.send_message(r#"{"t":"bye","reason":"O host removeu você da transmissão."}"#).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = p.peer.pc.close().await;
        });
    }
    emit_peers(&app, &peers);
}

fn stop_host(state: &RdvState) {
    rendezvous::host_stop(state);
    let session = HOST.lock().unwrap().take();
    if let Some(h) = session {
        h.video.stop();
        drop(h.audio);
        let peers: Vec<Arc<HostPeer>> = h.peers.lock().unwrap().drain().map(|(_, p)| p.peer).collect();
        transport::spawn(async move {
            for p in peers {
                p.send_message(r#"{"t":"bye","reason":"O host encerrou a transmissão."}"#).await;
                let _ = p.pc.close().await;
            }
        });
    }
}

#[tauri::command]
pub fn host_stop(state: State<'_, RdvState>) {
    stop_host(&state);
}

/* ========================================================================== */
/*                                 ESPECTADOR                                   */
/*      Várias transmissões ao mesmo tempo: cada uma é uma sessão com ID.        */
/* ========================================================================== */

struct ViewerState {
    gen: u64,
    stop: Arc<AtomicBool>,
    session: Option<Arc<ViewerSession>>,
    player: Option<Player>,
    playback: Option<Arc<Playback>>,
    volume: f32,
    muted: bool,
}

static VIEWERS: Mutex<Option<HashMap<String, ViewerState>>> = Mutex::new(None);
static VIEWER_GEN: AtomicU64 = AtomicU64::new(0);

fn with_viewers<T>(f: impl FnOnce(&mut HashMap<String, ViewerState>) -> T) -> T {
    let mut g = VIEWERS.lock().unwrap();
    f(g.get_or_insert_with(HashMap::new))
}

/// A sessão `id` ainda é a mesma tentativa `gen`?
fn viewer_alive(id: &str, gen: u64) -> bool {
    with_viewers(|m| m.get(id).is_some_and(|v| v.gen == gen))
}

#[derive(Serialize, Clone, Default)]
struct ViewerStats {
    id: String,
    width: u32,
    height: u32,
    fps: f32,
    kbps: u32,
    /// Atraso atual do buffer de reprodução (ms).
    buffer_ms: u32,
}

fn main_hwnd(app: &AppHandle) -> Res<isize> {
    let win = app.get_webview_window("main").ok_or("janela principal não encontrada")?;
    Ok(win.hwnd().map_err(|e| e.to_string())?.0 as isize)
}

fn emit_viewer(app: &AppHandle, event: &str, id: &str, value: serde_json::Value) {
    let _ = app.emit(event, serde_json::json!({ "id": id, "value": value }));
}

/// Entra numa transmissão pelo código. `id` identifica o tile na interface.
/// Volta com o nome do host quando a conexão é montada.
#[tauri::command]
pub async fn viewer_join(app: AppHandle, id: String, code: String, name: String, volume: u32) -> Res<String> {
    leave_viewer(&app, &id).await;
    let gen = VIEWER_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let stop = Arc::new(AtomicBool::new(false));
    let vol = volume.min(100) as f32 / 100.0;
    with_viewers(|m| {
        m.insert(id.clone(), ViewerState { gen, stop: stop.clone(), session: None, player: None, playback: None, volume: vol, muted: false });
    });
    let step = |s: &str| emit_viewer(&app, "viewer-step", &id, s.into());
    step("prepare");

    // Player nativo (janela-filha), criado na thread principal.
    let parent = main_hwnd(&app)?;
    let (ptx, prx) = std::sync::mpsc::channel::<Res<Player>>();
    let (app_main, tag) = (app.clone(), id.clone());
    app.run_on_main_thread(move || { let _ = ptx.send(Player::create(&app_main, parent, &tag)); }).map_err(|e| e.to_string())?;
    let player = prx.recv_timeout(Duration::from_secs(10)).map_err(|_| "o player não iniciou".to_string())??;
    let feed: PlayerFeed = player.feed();
    let playback = Arc::new(Playback::start()?);
    playback.set_volume(vol);

    // Decodificação numa thread própria.
    let (dtx, drx) = sync_channel::<(Vec<u8>, u32)>(90);
    let bytes_in = Arc::new(AtomicUsize::new(0));
    // Quadro perdido antes do decoder (fila cheia ou erro): os seguintes dependem dele,
    // então descartamos tudo até o próximo keyframe e pedimos um ao host. Sem isso a
    // imagem fica corrompida e só atualiza onde há movimento.
    let wait_idr = Arc::new(AtomicBool::new(false));
    let want_kf = Arc::new(AtomicBool::new(false));
    {
        let (app, bytes_in, id) = (app.clone(), bytes_in.clone(), id.clone());
        let (wait_idr, want_kf) = (wait_idr.clone(), want_kf.clone());
        std::thread::Builder::new()
            .name("aj-decode".into())
            .spawn(move || {
                if mf_init().is_err() { return; }
                let Ok(mut dec) = Decoder::new() else { return };
                // Timer de 1 ms para o buffer de reprodução acordar na hora certa.
                unsafe { windows::Win32::Media::timeBeginPeriod(1); }
                let mut playout = crate::playout::Playout::new();
                let (mut frames, mut win) = (0u32, Instant::now());
                let mut stats = ViewerStats { id, ..Default::default() };
                while let Ok((data, ts)) = drx.recv() {
                    if wait_idr.load(Ordering::SeqCst) && !crate::media::has_nal(&data, 5) {
                        continue; // restos de antes da perda, ainda na fila
                    }
                    let decoded = dec.decode(&data, 0);
                    if decoded.is_err() {
                        wait_idr.store(true, Ordering::SeqCst);
                        want_kf.store(true, Ordering::SeqCst);
                    }
                    if let Ok(out) = decoded {
                        for f in out {
                            stats.width = f.width;
                            stats.height = f.height;
                            frames += 1;
                            // Mostra no horário certo (absorve a variação da rede e do host).
                            crate::playout::sleep_until(playout.schedule(ts));
                            feed.push(f);
                        }
                    }
                    stats.buffer_ms = playout.delay_ms().round() as u32;
                    let el = win.elapsed();
                    if el >= Duration::from_secs(1) {
                        stats.fps = frames as f32 / el.as_secs_f32();
                        stats.kbps = (bytes_in.swap(0, Ordering::SeqCst) as f64 * 8.0 / el.as_secs_f64() / 1000.0) as u32;
                        let _ = app.emit("viewer-stats", stats.clone());
                        frames = 0;
                        win = Instant::now();
                    }
                }
                unsafe { windows::Win32::Media::timeEndPeriod(1); }
            })
            .map_err(|e| e.to_string())?;
    }

    let ok = with_viewers(|m| match m.get_mut(&id) {
        Some(v) if v.gen == gen => {
            v.player = Some(player);
            v.playback = Some(playback.clone());
            Ok(())
        }
        _ => Err(player),
    });
    if let Err(p) = ok {
        let _ = app.run_on_main_thread(move || p.destroy());
        return Err("cancelado".into());
    }

    let opus = Arc::new(Mutex::new(OpusDecoder::new()?));
    let sinks = {
        let (a1, a2, id1, id2) = (app.clone(), app.clone(), id.clone(), id.clone());
        let pb = playback.clone();
        let bytes_in = bytes_in.clone();
        let (wait_idr, want_kf) = (wait_idr.clone(), want_kf.clone());
        ViewerSinks {
            on_video: Arc::new(move |f, ts| {
                bytes_in.fetch_add(f.len(), Ordering::Relaxed);
                if wait_idr.load(Ordering::SeqCst) {
                    if !crate::media::has_nal(&f, 5) {
                        want_kf.store(true, Ordering::SeqCst);
                        return;
                    }
                    wait_idr.store(false, Ordering::SeqCst);
                }
                if dtx.try_send((f, ts)).is_err() {
                    wait_idr.store(true, Ordering::SeqCst);
                    want_kf.store(true, Ordering::SeqCst);
                }
            }),
            on_audio: Arc::new(move |p| pb.push(opus.lock().unwrap().decode(&p))),
            on_state: Arc::new(move |e| {
                let s = match e {
                    PeerEvent::Connected => "connected",
                    PeerEvent::Disconnected => "unstable",
                    PeerEvent::Failed => "failed",
                    PeerEvent::Closed => "closed",
                };
                emit_viewer(&a1, "viewer-state", &id1, s.into());
            }),
            on_message: Arc::new(move |m| {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&m) {
                    if v["t"] == "bye" {
                        let reason = v["reason"].as_str().unwrap_or("O host encerrou a transmissão.").to_string();
                        emit_viewer(&a2, "viewer-ended", &id2, reason.into());
                    }
                }
            }),
        }
    };

    let (session, offer) = transport::tokio_rt()
        .spawn(ViewerSession::offer(sinks, true))
        .await
        .map_err(|e| e.to_string())??;
    if !viewer_alive(&id, gen) {
        let _ = transport::tokio_rt().spawn(async move { let _ = session.pc.close().await; }).await;
        return Err("cancelado".into());
    }
    with_viewers(|m| if let Some(v) = m.get_mut(&id) { v.session = Some(session.clone()); });

    step("publish");
    let mut sid_bytes = [0u8; 12];
    getrandom::fill(&mut sid_bytes).map_err(|e| e.to_string())?;
    let sid: String = sid_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let payload = serde_json::json!({ "t": "offer", "sid": sid, "sdp": offer, "name": name }).to_string();
    let (app2, id_wait, stop2) = (app.clone(), id.clone(), stop.clone());
    let answer = tokio::task::spawn_blocking(move || {
        let state = app2.state::<RdvState>();
        rendezvous::join(&state, &code, &sid, &payload, JOIN_TIMEOUT_S, &stop2, || {
            emit_viewer(&app2, "viewer-step", &id_wait, "wait".into())
        })
    })
    .await
    .map_err(|e| e.to_string())??;
    if !viewer_alive(&id, gen) {
        return Err("cancelado".into());
    }

    let answer: serde_json::Value = serde_json::from_str(&answer).map_err(|_| "O host mandou uma resposta inválida.")?;
    let sdp = answer["sdp"].as_str().ok_or("O host mandou uma resposta inválida.")?.to_string();
    let host_name: String = answer["name"].as_str().unwrap_or("Host").chars().take(40).collect();
    step("connect");
    let s = session.clone();
    transport::tokio_rt().spawn(async move { s.accept_answer(sdp).await }).await.map_err(|e| e.to_string())??;

    // Ping até o host (uma vez por segundo) e pedidos de keyframe do decoder.
    let (s, app_ping, id_ping) = (session.clone(), app.clone(), id.clone());
    transport::spawn(async move {
        let mut last_kf = Instant::now() - Duration::from_secs(5);
        for tick in 1u64.. {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if !viewer_alive(&id_ping, gen) {
                break;
            }
            if want_kf.load(Ordering::SeqCst) && last_kf.elapsed() > Duration::from_millis(500) {
                want_kf.store(false, Ordering::SeqCst);
                last_kf = Instant::now();
                s.send_message(r#"{"t":"kf"}"#).await;
            }
            if tick % 4 == 0 {
                if let Some(ms) = s.rtt_ms().await {
                    emit_viewer(&app_ping, "viewer-ping", &id_ping, (ms.round() as u32).into());
                }
            }
        }
    });
    Ok(host_name)
}

async fn leave_viewer(app: &AppHandle, id: &str) {
    let Some(v) = with_viewers(|m| m.remove(id)) else { return };
    v.stop.store(true, Ordering::SeqCst);
    drop(v.playback);
    if let Some(s) = v.session {
        let _ = transport::tokio_rt()
            .spawn(async move {
                s.send_message(r#"{"t":"bye"}"#).await;
                let _ = s.pc.close().await;
            })
            .await;
    }
    if let Some(p) = v.player {
        let _ = app.run_on_main_thread(move || p.destroy());
    }
}

#[tauri::command]
pub async fn viewer_leave(app: AppHandle, id: String) {
    leave_viewer(&app, &id).await;
}

/// Área de vídeo de uma stream em pixels físicos (relativos à janela). Thread principal.
#[tauri::command]
pub fn player_rect(id: String, x: i32, y: i32, width: i32, height: i32, visible: bool) {
    with_viewers(|m| {
        if let Some(p) = m.get(&id).and_then(|v| v.player.as_ref()) {
            p.set_rect(x, y, width, height, visible);
        }
    });
}

/// Volume de 0 a 100 de uma stream.
#[tauri::command]
pub fn viewer_volume(id: String, volume: u32, muted: bool) {
    with_viewers(|m| {
        if let Some(v) = m.get_mut(&id) {
            v.volume = volume.min(100) as f32 / 100.0;
            v.muted = muted;
            if let Some(pb) = &v.playback {
                pb.set_volume(if muted { 0.0 } else { v.volume });
            }
        }
    });
}
