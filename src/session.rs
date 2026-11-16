//! Sessões de transmissão (host) e de assistir (espectador), expostas como comandos Tauri.

use crate::audio::{self, AudioCapture, OpusDecoder, OpusEncoder, Playback};
use crate::congestion::{self, RateController, Report};
use crate::capture::{self, Source};
use crate::media::{mf_init, Decoder};
use crate::player::{Player, PlayerFeed};
use crate::rendezvous::{self, RdvState};
use crate::transport::{self, HostPeer, PeerEvent, ViewerSession, ViewerSinks};
use crate::video::{Callbacks, ExtraReq, VideoPipeline, VideoSettings};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use crate::app::AppHandle;

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
    /// Senha da transmissão (vazia = sem senha).
    #[serde(default)]
    pub password: String,
}

impl HostConfig {
    fn password(&self) -> Option<&str> {
        Some(self.password.as_str()).filter(|p| !p.is_empty())
    }

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
    /// Tentativa (da oferta) que criou esta conexão.
    sid: String,
    /// Quanto a rede até este espectador aguenta (pelos relatórios dele).
    rate: RateController,
    /// Qualidade pedida: None = automática; Some(altura).
    quality: Option<u32>,
    /// Versão que ele recebe agora (0 = principal, senão a altura da versão menor) e a
    /// próxima, que só começa num keyframe dela (senão a imagem corromperia).
    rendition: u32,
    pending: Option<u32>,
    joined: Instant,
    /// Automático querendo subir de versão: qual e desde quando (sobe só depois de um tempo).
    up_since: Option<(u32, Instant)>,
}

/// Keyframes forçados: no máximo um por intervalo em cada versão, somando os pedidos de
/// todos os espectadores dela. Antes cada espectador podia pedir a cada 300 ms.
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_secs(1);
/// Espectador que já mandou relatórios e parou por esse tempo: a conexão dele está
/// tão ruim que nem o relatório chega. Conta como rede sobrecarregada.
const REPORT_SILENCE: Duration = Duration::from_millis(1500);
/// Qualidade automática: espera a estimativa de banda firmar antes de trocar de versão,
/// e só sobe depois de a folga durar esse tempo (descer é na hora).
const AUTO_SETTLE: Duration = Duration::from_secs(8);
const AUTO_UP_AFTER: Duration = Duration::from_secs(5);

/// O que as partes do host compartilham: encoder, espectadores, keyframes e bitrate.
struct HostShared {
    video: VideoPipeline,
    peers: Arc<Mutex<HashMap<u64, PeerEntry>>>,
    /// Versões que precisam de keyframe (0 = principal, senão a altura).
    keyframes: Arc<Mutex<std::collections::HashSet<u32>>>,
    /// Bitrate alvo escolhido por quem transmite (b/s), informado pelo pipeline.
    user_bps: Arc<AtomicU32>,
    /// Altura da versão principal e as menores possíveis (altura, bitrate alvo).
    renditions: Arc<Mutex<(u32, Vec<(u32, u32)>)>>,
    /// Limite de bitrate pela rede aplicado agora na principal (None = sem limite).
    applied: Mutex<Option<u32>>,
    /// Versões menores pedidas ao pipeline agora.
    extras: Mutex<Vec<ExtraReq>>,
    alive: AtomicBool,
}

impl HostShared {
    fn request_keyframe(&self, key: u32) {
        self.keyframes.lock().unwrap().insert(key);
    }

    /// Keyframe na versão que o espectador `id` recebe (ou vai receber).
    fn request_keyframe_for(&self, id: u64) {
        let key = self.peers.lock().unwrap().get(&id).map(|p| p.pending.unwrap_or(p.rendition)).unwrap_or(0);
        self.request_keyframe(key);
    }

    fn user_bps(&self) -> u32 {
        self.user_bps.load(Ordering::Relaxed).max(congestion::MIN_BPS)
    }

    /// Versões disponíveis agora: (chave, bitrate alvo), da melhor para a pior.
    fn options(&self) -> Vec<(u32, u32)> {
        let (_, extras) = self.renditions.lock().unwrap().clone();
        std::iter::once((0, self.user_bps())).chain(extras).collect()
    }

    /// Mensagem com as qualidades que quem assiste pode escolher.
    fn renditions_msg(&self) -> String {
        let (main, extras) = self.renditions.lock().unwrap().clone();
        let heights: Vec<u32> = std::iter::once(main).chain(extras.iter().map(|(h, _)| *h)).filter(|h| *h > 0).collect();
        serde_json::json!({ "t": "renditions", "h": heights }).to_string()
    }

    fn on_report(&self, id: u64, r: &Report) {
        let max = self.user_bps();
        if let Some(p) = self.peers.lock().unwrap().get_mut(&id) {
            p.rate.on_report(r, max, Instant::now());
        }
        self.assign();
        self.apply();
    }

    fn set_quality(&self, id: u64, q: &str) {
        if let Some(p) = self.peers.lock().unwrap().get_mut(&id) {
            p.quality = q.parse().ok();
            p.up_since = None;
        }
        self.assign();
        self.apply();
    }

    /// Escolhe a versão de cada espectador (pedida ou automática). Uma troca só vale no
    /// próximo keyframe da versão nova, que é pedido aqui.
    fn assign(&self) {
        let options = self.options();
        let main_height = self.renditions.lock().unwrap().0;
        let now = Instant::now();
        let mut keyframes = vec![];
        for p in self.peers.lock().unwrap().values_mut() {
            if p.state == "connecting" {
                continue;
            }
            let current = p.pending.unwrap_or(p.rendition);
            let want = match p.quality {
                // Fixa: a maior versão que não passa da pedida (a principal se pediu ela ou mais).
                Some(h) if h >= main_height => 0,
                Some(h) => options.iter().skip(1).find(|(eh, _)| *eh <= h).or(options.last()).map(|(k, _)| *k).unwrap_or(0),
                None => {
                    let settled = now.saturating_duration_since(p.joined) >= AUTO_SETTLE && p.rate.last_report().is_some();
                    if !settled || options.len() < 2 {
                        current
                    } else {
                        let pick = congestion::pick_rendition(&options, p.rate.estimate());
                        let rank = |k: u32| options.iter().position(|(o, _)| *o == k).unwrap_or(0);
                        if rank(pick) > rank(current) {
                            p.up_since = None;
                            pick // pior conexão: desce na hora
                        } else if rank(pick) < rank(current) {
                            match p.up_since {
                                Some((k, t)) if k == pick && now.saturating_duration_since(t) >= AUTO_UP_AFTER => pick,
                                Some((k, _)) if k == pick => current,
                                _ => {
                                    p.up_since = Some((pick, now));
                                    current
                                }
                            }
                        } else {
                            p.up_since = None;
                            current
                        }
                    }
                }
            };
            // A versão sumiu (ex.: a principal ficou menor que ela): volta para a principal.
            let want = if want == 0 || options.iter().any(|(k, _)| *k == want) { want } else { 0 };
            if want != current {
                p.up_since = None;
                if want == p.rendition {
                    p.pending = None;
                } else {
                    p.pending = Some(want);
                    keyframes.push(want);
                }
            }
        }
        for k in keyframes {
            self.request_keyframe(k);
        }
    }

    /// Limites de bitrate pela rede: cada versão segue a pior conexão entre quem a usa.
    /// Aplica no encoder de cada versão e nos pacers.
    fn apply(&self) {
        let now = Instant::now();
        let user = self.user_bps();
        let options = self.options();
        let peers = self.peers.lock().unwrap();
        let on = |k: u32| peers.values().filter(move |p| p.state != "connecting" && p.rendition == k).map(|p| &p.rate);
        let main_cap = congestion::combined_target(on(0), now).filter(|&t| t < user);
        let mut reqs: Vec<ExtraReq> = vec![];
        for (h, bps) in options.iter().skip(1) {
            let used = peers.values().any(|p| p.state != "connecting" && (p.rendition == *h || p.pending == Some(*h)));
            if used {
                reqs.push(ExtraReq { height: *h, cap_bps: congestion::combined_target(on(*h), now).filter(|t| t < bps) });
            }
        }
        for p in peers.values() {
            let target = match p.rendition {
                0 => main_cap.unwrap_or(user),
                h => reqs.iter().find(|r| r.height == h).and_then(|r| r.cap_bps).or(options.iter().find(|(k, _)| *k == h).map(|(_, b)| *b)).unwrap_or(user),
            };
            p.peer.set_target_bitrate(target);
        }
        drop(peers);
        let mut applied = self.applied.lock().unwrap();
        let changed = match (*applied, main_cap) {
            (Some(a), Some(t)) => a.abs_diff(t) > a / 33, // ignora mudanças de menos de 3%
            (a, t) => a != t,
        };
        if changed {
            *applied = main_cap;
            self.video.set_network(main_cap);
        }
        drop(applied);
        let mut extras = self.extras.lock().unwrap();
        let differs = extras.len() != reqs.len()
            || extras.iter().zip(&reqs).any(|(a, b)| {
                a.height != b.height
                    || match (a.cap_bps, b.cap_bps) {
                        (Some(x), Some(y)) => x.abs_diff(y) > x / 33,
                        (x, y) => x != y,
                    }
            });
        if differs {
            *extras = reqs.clone();
            self.video.set_extras(reqs);
        }
    }

    /// A cada ~500 ms: quem parou de mandar relatório conta como sobrecarga.
    fn tick(&self) {
        let now = Instant::now();
        let max = self.user_bps();
        for p in self.peers.lock().unwrap().values_mut() {
            if p.state != "connecting" && p.rate.last_report().is_some_and(|t| now.saturating_duration_since(t) > REPORT_SILENCE) {
                p.rate.on_report(&Report { kbps: 0, loss: 1.0, qd: 1000.0 }, max, now);
            }
        }
        self.assign();
        self.apply();
    }
}

enum Media {
    /// (versão: 0 = principal, senão a altura; dados; duração)
    Video(u32, Bytes, Duration),
    Audio(Bytes),
}

struct HostSession {
    code: String,
    config: HostConfig,
    shared: Arc<HostShared>,
    video: VideoPipeline,
    audio: Option<AudioCapture>,
    opus: Arc<Mutex<OpusEncoder>>,
    peers: Arc<Mutex<HashMap<u64, PeerEntry>>>,
    media_tx: tokio::sync::mpsc::Sender<Media>,
    on_offer: rendezvous::OfferFn,
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
        locked: cfg.password().is_some(),
    };
    rendezvous::PublicAnnouncer::start(state, entry).ok()
}

static HOST: Mutex<Option<HostSession>> = Mutex::new(None);

/// Ofertas já vistas, por `sid`: `None` enquanto a resposta é montada, depois (slot, resposta).
type Offers = Arc<Mutex<HashMap<String, Option<(u8, String)>>>>;
static PEER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Máximo de espectadores entrando ao mesmo tempo (resposta sendo montada ou ainda
/// "conectando"). Cada um segura uma PeerConnection por até ~2 min; sem limite, quem
/// tem o código poderia mandar ofertas falsas sem parar e esgotar memória e portas.
/// Ofertas recusadas não são marcadas como vistas: o espectador republica a cada 25 s.
const MAX_PENDING: usize = 6;

/// Esquece as ofertas cujas conexões já acabaram e conta as que ainda estão entrando.
fn prune_offers(map: &mut HashMap<String, Option<(u8, String)>>, peers: &Mutex<HashMap<u64, PeerEntry>>) -> usize {
    let peers = peers.lock().unwrap();
    let live: Vec<(&str, bool)> = peers.values().map(|p| (p.sid.as_str(), p.state == "connecting")).collect();
    prune_offers_by(map, &live)
}

/// `live` = (sid, ainda conectando?) de cada conexão existente.
fn prune_offers_by<T>(map: &mut HashMap<String, Option<T>>, live: &[(&str, bool)]) -> usize {
    let is = |sid: &str, connecting_only: bool| live.iter().any(|&(s, c)| s == sid && (c || !connecting_only));
    map.retain(|sid, v| v.is_none() || is(sid, false));
    map.iter().filter(|(sid, v)| v.is_none() || is(sid, true)).count()
}

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
pub fn sources_list() -> Vec<Source> {
    capture::list_sources()
}

/// Começa a transmitir e devolve o código.
pub fn host_start(app: AppHandle, config: HostConfig) -> Res<String> {
    let state = app.rdv();
    stop_host(&state);
    // Malha P2P, etapa 1 (docs/malha-p2p.md): vídeo em pedaços pelo canal de dados, para
    // quem tem esse canal. Por enquanto só com AJ_PIECES=1.
    transport::USE_PIECES.store(std::env::var_os("AJ_PIECES").is_some(), Ordering::Relaxed);
    let (media_tx, mut media_rx) = tokio::sync::mpsc::channel::<Media>(240);
    let peers: Arc<Mutex<HashMap<u64, PeerEntry>>> = Arc::new(Mutex::new(HashMap::new()));
    let keyframes: Arc<Mutex<std::collections::HashSet<u32>>> = Default::default();
    let user_bps = Arc::new(AtomicU32::new(0));
    let renditions: Arc<Mutex<(u32, Vec<(u32, u32)>)>> = Default::default();

    // Distribui os pacotes para os espectadores, cada um na sua versão.
    {
        let peers = peers.clone();
        let kf = keyframes.clone();
        transport::spawn(async move {
            // Malha (etapa 2): cada quadro vira pedaços uma vez só, os mesmos para todos.
            let mut source = crate::pieces::PieceSource::new(transport::PIECE_OVERHEAD);
            let use_pieces = transport::USE_PIECES.load(Ordering::Relaxed);
            while let Some(m) = media_rx.recv().await {
                let targets: Vec<Arc<HostPeer>> = {
                    let mut peers = peers.lock().unwrap();
                    let (key, is_keyframe) = match &m {
                        Media::Video(k, d, _) => (Some(*k), crate::media::has_nal(d, 5)),
                        Media::Audio(_) => (None, false),
                    };
                    peers
                        .values_mut()
                        .filter(|p| p.state != "connecting")
                        .filter_map(|p| {
                            let Some(k) = key else { return Some(p.peer.clone()) };
                            // Troca de versão: começa exatamente num keyframe da nova.
                            if p.pending == Some(k) && is_keyframe {
                                p.rendition = k;
                                p.pending = None;
                            }
                            (p.rendition == k).then(|| p.peer.clone())
                        })
                        .collect()
                };
                let pieces = match &m {
                    Media::Video(k, d, dur) if use_pieces => source.encode(*k, d, *dur),
                    _ => None,
                };
                for p in targets {
                    match &m {
                        Media::Video(k, d, dur) => {
                            // Fila do pacer cheia: quadro descartado, quem perdeu precisa de keyframe.
                            let sent = match &pieces {
                                Some(f) if p.has_pieces() => p.send_pieces(f),
                                _ => !matches!(p.send_video(d.clone(), *dur).await, Ok(false)),
                            };
                            if !sent {
                                kf.lock().unwrap().insert(*k);
                            }
                        }
                        Media::Audio(d) => {
                            let _ = p.send_audio(d.clone(), Duration::from_millis(20)).await;
                        }
                    }
                }
            }
        });
    }

    // Pipeline de vídeo.
    let video = {
        let (a1, a2, a3, a4) = (app.clone(), app.clone(), app.clone(), app.clone());
        let tx = media_tx.clone();
        let kf = keyframes.clone();
        let ub = user_bps.clone();
        let (rd, pr) = (renditions.clone(), peers.clone());
        VideoPipeline::start(
            config.source.clone(),
            config.video(),
            Callbacks {
                on_packet: Arc::new(move |key, data, dur| {
                    if tx.try_send(Media::Video(key, Bytes::from(data), dur)).is_err() {
                        kf.lock().unwrap().insert(key); // fila cheia: quadro perdido
                    }
                }),
                on_info: Arc::new(move |i| {
                    ub.store(i.target_kbps * 1000, Ordering::Relaxed);
                    // Qualidades disponíveis mudaram: avisa quem assiste (para o menu).
                    let now = (i.height, i.extras.iter().copied().zip(i.extras_bps.iter().copied()).collect::<Vec<_>>());
                    let changed = {
                        let mut r = rd.lock().unwrap();
                        let heights_changed = r.0 != now.0 || r.1.iter().map(|x| x.0).ne(now.1.iter().map(|x| x.0));
                        *r = now.clone();
                        heights_changed
                    };
                    if changed {
                        let heights: Vec<u32> = std::iter::once(now.0).chain(now.1.iter().map(|x| x.0)).collect();
                        let msg = serde_json::json!({ "t": "renditions", "h": heights }).to_string();
                        let targets: Vec<Arc<HostPeer>> = pr.lock().unwrap().values().map(|p| p.peer.clone()).collect();
                        transport::spawn(async move {
                            for p in targets {
                                p.send_message(&msg).await;
                            }
                        });
                    }
                    let _ = a1.emit("host-info", i);
                }),
                on_preview: Arc::new(move |j| { let _ = a2.emit("host-preview", j); }),
                on_error: Arc::new(move |e| { let _ = a3.emit("host-error", e); }),
                on_source_closed: Arc::new(move || { let _ = a4.emit("host-source-closed", ()); }),
            },
        )?
    };

    let shared = Arc::new(HostShared {
        video: video.clone(),
        peers: peers.clone(),
        keyframes: keyframes.clone(),
        user_bps,
        renditions,
        applied: Mutex::new(None),
        extras: Mutex::new(vec![]),
        alive: AtomicBool::new(true),
    });

    let opus = Arc::new(Mutex::new(OpusEncoder::new(config.abitrate.clamp(32, 510))?));
    opus.lock().unwrap().set_gain(config.volume as f32 / 100.0);
    let audio = start_audio(&app, &config, opus.clone(), media_tx.clone());

    // Pedidos de conexão vindos da DHT.
    let on_offer: rendezvous::OfferFn = {
        let app = app.clone();
        let peers = peers.clone();
        let shared = shared.clone();
        let offers: Offers = Arc::new(Mutex::new(HashMap::new()));
        Arc::new(move |code: &str, slot: u8, payload: String| {
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
                    let (reply_slot, answer, app, code) = (*reply_slot, answer.clone(), app.clone(), code.to_string());
                    drop(map);
                    std::thread::spawn(move || {
                        let _ = rendezvous::host_reply(&app.rdv(), &code, reply_slot, &answer);
                    });
                }
                Some(None) => {} // ainda montando a resposta
                None => {
                    if prune_offers(&mut map, &peers) >= MAX_PENDING {
                        return; // muita gente entrando agora; o espectador tenta de novo
                    }
                    map.insert(sid.to_string(), None);
                    drop(map);
                    let name: String = msg["name"].as_str().unwrap_or("Espectador").chars().take(40).collect();
                    let (app, peers, shared, offers) = (app.clone(), peers.clone(), shared.clone(), offers.clone());
                    let (sdp, sid, code) = (sdp.to_string(), sid.to_string(), code.to_string());
                    transport::spawn(async move { accept_viewer(app, peers, shared, offers, code, slot, sid, sdp, name).await });
                }
            }
        })
    };

    let code = rendezvous::host_listen(&state, on_offer.clone(), config.password())?;
    let since = rendezvous::now_ms();
    let public = start_public(&state, &config, &code, since, 0);
    *HOST.lock().unwrap() = Some(HostSession { code: code.clone(), config, shared: shared.clone(), video, audio, opus, peers, media_tx, on_offer, public, since });

    // Keyframes pedidos (no máximo 1 por segundo em cada versão) e bitrate de quem parou
    // de mandar relatório.
    transport::spawn(async move {
        let mut last_kf: HashMap<u32, Instant> = HashMap::new();
        let mut last_tick = Instant::now();
        while shared.alive.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let due: Vec<u32> = {
                let mut wanted = shared.keyframes.lock().unwrap();
                let due: Vec<u32> = wanted.iter().copied().filter(|k| last_kf.get(k).is_none_or(|t| t.elapsed() >= KEYFRAME_MIN_INTERVAL)).collect();
                for k in &due {
                    wanted.remove(k);
                }
                due
            };
            for k in due {
                last_kf.insert(k, Instant::now());
                shared.video.keyframe_for(k);
            }
            if last_tick.elapsed() >= congestion::REPORT_INTERVAL {
                last_tick = Instant::now();
                shared.tick();
            }
        }
    });
    Ok(code)
}

async fn accept_viewer(
    app: AppHandle,
    peers: Arc<Mutex<HashMap<u64, PeerEntry>>>,
    shared: Arc<HostShared>,
    offers: Offers,
    // Código em que o pedido chegou: a resposta vai por ele, mesmo que o host
    // tenha gerado um código novo nesse meio tempo.
    code: String,
    slot: u8,
    sid: String,
    sdp: String,
    name: String,
) {
    let id = PEER_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let on_state: Arc<dyn Fn(PeerEvent) + Send + Sync> = {
        let (app, peers, shared, name) = (app.clone(), peers.clone(), shared.clone(), name.clone());
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
                            // As qualidades que dá para escolher (menu de quem assiste).
                            let (peer, msg) = (p.peer.clone(), shared.renditions_msg());
                            transport::spawn(async move { peer.send_message(&msg).await });
                        }
                        // (a lista está travada aqui: não chamar o que trava de novo)
                        shared.request_keyframe(map.get(&id).map(|p| p.rendition).unwrap_or(0));
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
        let (shared, peers, app) = (shared.clone(), peers.clone(), app.clone());
        Arc::new(move |m: String| {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&m) else { return };
            if v["t"] == "kf" {
                shared.request_keyframe_for(id);
            } else if v["t"] == "quality" {
                shared.set_quality(id, v["q"].as_str().unwrap_or("auto"));
            } else if v["t"] == "rr" {
                if let Ok(r) = serde_json::from_value::<Report>(v) {
                    shared.on_report(id, &r);
                }
            } else if v["t"] == "bye" {
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
    let rate = RateController::new(shared.user_bps());
    peer.set_target_bitrate(shared.applied.lock().unwrap().unwrap_or(shared.user_bps()));
    peers.lock().unwrap().insert(
        id,
        PeerEntry {
            name: name.clone(),
            state: "connecting",
            peer: peer.clone(),
            sid: sid.clone(),
            rate,
            quality: None,
            rendition: 0,
            pending: None,
            joined: Instant::now(),
            up_since: None,
        },
    );
    emit_peers(&app, &peers);

    let host_name = HOST.lock().unwrap().as_ref().map(|h| h.config.name.clone()).unwrap_or_default();
    let payload = serde_json::json!({ "t": "answer", "sid": sid, "sdp": answer, "name": host_name }).to_string();
    offers.lock().unwrap().insert(sid.clone(), Some((slot, payload.clone())));
    let app2 = app.clone();
    // Se a publicação falhar, o espectador republica a oferta e a resposta vai de novo.
    let _ = tokio::task::spawn_blocking(move || {
        rendezvous::host_reply(&app2.rdv(), &code, slot, &payload)
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
pub fn host_update(app: AppHandle, config: HostConfig) -> Res<()> {
    let state = app.rdv();
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
pub fn host_new_code(app: &AppHandle) -> Res<String> {
    let state = app.rdv();
    let (on_offer, password) = HOST
        .lock()
        .unwrap()
        .as_ref()
        .map(|h| (h.on_offer.clone(), h.config.password.clone()))
        .ok_or("não está transmitindo")?;
    let password = Some(password.as_str()).filter(|p| !p.is_empty());
    let code = rendezvous::host_listen(&state, on_offer, password)?;
    if let Some(h) = HOST.lock().unwrap().as_mut() {
        h.code = code.clone();
        if let Some(a) = &h.public {
            let (c, locked) = (code.clone(), password.is_some());
            a.update(|e| { e.code = c; e.locked = locked; }, true);
        }
    }
    Ok(code)
}

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
        h.shared.alive.store(false, Ordering::SeqCst);
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

pub fn host_stop(app: &AppHandle) {
    stop_host(app.rdv());
}

/* ========================================================================== */
/*                                 ESPECTADOR                                   */
/*      Várias transmissões ao mesmo tempo: cada uma é uma sessão com ID.        */
/* ========================================================================== */

struct ViewerState {
    gen: u64,
    /// fps escolhido para assistir (0 = o da stream; lido pela thread de decodificação).
    view_fps: Arc<AtomicU32>,
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
    /// Quadros mostrados por segundo (difere de `fps` com fps escolhido para assistir).
    shown_fps: f32,
}

fn main_hwnd(app: &AppHandle) -> Res<isize> {
    match app.hwnd() {
        0 => Err("janela principal não encontrada".into()),
        h => Ok(h),
    }
}

fn emit_viewer(app: &AppHandle, event: &str, id: &str, value: serde_json::Value) {
    let _ = app.emit(event, serde_json::json!({ "id": id, "value": value }));
}

/// Entra numa transmissão pelo código. `id` identifica o tile na interface.
/// Volta com o nome do host quando a conexão é montada.
pub async fn viewer_join(app: AppHandle, id: String, code: String, password: Option<String>, name: String, volume: u32, view_fps: u32) -> Res<String> {
    leave_viewer(&app, &id).await;
    let gen = VIEWER_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let stop = Arc::new(AtomicBool::new(false));
    let vol = volume.min(100) as f32 / 100.0;
    let view_fps = Arc::new(AtomicU32::new(view_fps));
    with_viewers(|m| {
        m.insert(id.clone(), ViewerState { gen, view_fps: view_fps.clone(), stop: stop.clone(), session: None, player: None, playback: None, volume: vol, muted: false });
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
        let view_fps = view_fps.clone();
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
                let mut kbps_avg = 0.0f64;
                let mut presenter = crate::framegen::Presenter::new();
                let mut shown = 0u32;
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
                            // Horário certo (absorve a variação da rede e do host), no fps
                            // escolhido para assistir (descarta, mostra ou gera quadros).
                            let at = playout.schedule(ts);
                            shown += presenter.present(f, ts, at, view_fps.load(Ordering::Relaxed), &feed);
                        }
                    }
                    stats.buffer_ms = playout.delay_ms().round() as u32;
                    let el = win.elapsed();
                    if el >= Duration::from_secs(1) {
                        stats.fps = frames as f32 / el.as_secs_f32();
                        stats.shown_fps = shown as f32 / el.as_secs_f32();
                        shown = 0;
                        // Média móvel de ~3 s: a taxa real pula a cada keyframe e com o
                        // movimento da imagem; o número na tela não precisa pular junto.
                        let now_kbps = bytes_in.swap(0, Ordering::SeqCst) as f64 * 8.0 / el.as_secs_f64() / 1000.0;
                        kbps_avg = if kbps_avg == 0.0 { now_kbps } else { kbps_avg * 0.65 + now_kbps * 0.35 };
                        stats.kbps = kbps_avg as u32;
                        let _ = app.emit("viewer-stats", stats.clone());
                        let (proc_max, late_max) = presenter.take_stats();
                        if std::env::var_os("AJ_TRACE").is_some() {
                            eprintln!(
                                "[decode] {:.0} fps da stream, {:.0} na tela, buffer {} ms, gerador máx {proc_max:.1} ms, chegada atrasada máx {late_max:.1} ms, {}x{}",
                                stats.fps, stats.shown_fps, stats.buffer_ms, stats.width, stats.height
                            );
                        }
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
                    // Qualidades que o host oferece (menu de engrenagem).
                    if v["t"] == "renditions" {
                        emit_viewer(&a2, "viewer-renditions", &id2, v["h"].clone());
                    }
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
    let has_password = password.is_some();
    let answer = tokio::task::spawn_blocking(move || {
        let state = app2.rdv();
        rendezvous::join(&state, &code, password.as_deref(), &sid, &payload, JOIN_TIMEOUT_S, &stop2, || {
            emit_viewer(&app2, "viewer-step", &id_wait, "wait".into())
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| {
        // Com senha errada o host nem vê o pedido: o sintoma é ele não responder.
        if has_password && e.starts_with("O host não respondeu") {
            "O host não respondeu. Confira a senha e se a transmissão ainda está ativa.".to_string()
        } else {
            e
        }
    })?;
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
            if want_kf.load(Ordering::SeqCst) && last_kf.elapsed() > KEYFRAME_MIN_INTERVAL {
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

pub async fn viewer_leave(app: AppHandle, id: String) {
    leave_viewer(&app, &id).await;
}

/// Área de vídeo de uma stream em pixels físicos (relativos à janela). Thread principal.
/// `hole`: retângulo (relativo ao vídeo) recortado para um menu aparecer por cima.
pub fn player_rect(id: String, x: i32, y: i32, width: i32, height: i32, visible: bool, hole: Option<(i32, i32, i32, i32, i32)>) {
    with_viewers(|m| {
        if let Some(p) = m.get(&id).and_then(|v| v.player.as_ref()) {
            p.set_rect(x, y, width, height, visible);
            p.set_hole(hole);
        }
    });
}

/// Qualidade pedida ao host para uma stream ("auto" ou a altura, ex. "720").
pub fn viewer_quality(id: &str, quality: &str) {
    let session = with_viewers(|m| m.get(id).and_then(|v| v.session.clone()));
    if let Some(s) = session {
        let msg = serde_json::json!({ "t": "quality", "q": quality }).to_string();
        transport::spawn(async move { s.send_message(&msg).await });
    }
}

/// fps para assistir uma stream (0 = o da stream).
pub fn viewer_view_fps(id: &str, fps: u32) {
    with_viewers(|m| {
        if let Some(v) = m.get(id) {
            v.view_fps.store(fps, Ordering::Relaxed);
        }
    });
}

/// Esta máquina consegue gerar quadros (placa NVIDIA com Optical Flow)? Demora na
/// primeira chamada (inicia o CUDA): chamar fora da thread da interface.
pub fn framegen_available() -> bool {
    crate::nvof::OpticalFlow::available()
}

/// Volume de 0 a 100 de uma stream.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_offers_are_counted_and_pruned() {
        let mut map: HashMap<String, Option<u8>> = HashMap::new();
        map.insert("montando".into(), None); // resposta sendo montada
        map.insert("conectando".into(), Some(1)); // respondida, ainda conectando
        map.insert("assistindo".into(), Some(2)); // já conectou
        map.insert("saiu".into(), Some(3)); // a conexão já acabou
        let live = [("conectando", true), ("assistindo", false)];
        assert_eq!(prune_offers_by(&mut map, &live), 2);
        assert!(!map.contains_key("saiu"));
        assert!(map.contains_key("assistindo") && map.contains_key("montando"));
    }
}
