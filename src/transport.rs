//! Transporte WebRTC nativo (webrtc-rs): conexão P2P com NAT traversal, DTLS-SRTP,
//! H.264 + Opus. A troca de oferta/resposta continua pela DHT (módulo `rendezvous`).
//!
//! - Host: uma `PeerConnection` por espectador, com uma faixa de vídeo e uma de áudio.
//!   O encoder roda uma vez só e os pacotes são distribuídos para todos.
//! - Espectador: recebe RTP, remonta os quadros H.264 e entrega Opus cru.
//! - Canal de dados "aj": pedidos de keyframe e aviso de saída.
//! - Canal de dados "aj-p" (malha P2P, etapa 1; docs/malha-p2p.md): o vídeo em pedaços
//!   codificados (`pieces`), fora de ordem e sem reenvio, no lugar da faixa RTP de vídeo.
//!   Liga com USE_PIECES; espectador sem esse canal (versão antiga) continua no RTP.

use bytes::{Bytes, BytesMut};
use rtc::interceptor::Registry;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCIceServer;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::packetizer::{Depacketizer, Packetizer};
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::rtp_transceiver::{PayloadType, RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState, RTCPeerConnectionState,
};
use webrtc::runtime::{channel, default_runtime, Runtime, Sender};

pub type Res<T> = Result<T, String>;
pub(crate) fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

const H264_PT: u8 = 102;

/// Contadores de diagnóstico do receptor (quadros descartados e pedidos de keyframe).
pub static BROKEN_FRAMES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub static KEYFRAME_REQUESTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Só para testes: embaralha a ordem de chegada (por mil pacotes), simulando rede real.
pub static TEST_REORDER_PERMILLE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Pedidos de reparo atendidos pelo host (diagnóstico e testes).
pub static REPAIRS_SENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Testes: descarta essa fração (por mil) dos pedaços que chegam ao espectador.
pub static TEST_PIECE_LOSS_PERMILLE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const OPUS_PT: u8 = 111;
const ICE_TIMEOUT: Duration = Duration::from_secs(4);
const RTP_MTU: usize = 1200;
/// Ritmo do pacer em relação ao bitrate alvo: folga para keyframes (o libwebrtc usa 2,5x).
const PACING_FACTOR: f64 = 2.5;
const MIN_PACING_BPS: u64 = 2_000_000;
/// Rajada máxima que o pacer solta de uma vez depois de ficar parado.
pub(crate) const PACER_BURST: Duration = Duration::from_millis(5);
/// Quadros esperando no pacer (~0,75 s a 60 fps). Cheio: a rede não está dando conta.
const PACER_QUEUE: usize = 45;
/// Quanto uma mensagem de controle espera o canal abrir antes de desistir.
const CONTROL_WAIT: Duration = Duration::from_secs(5);

/// Manda texto pelo canal, esperando ele abrir (até `CONTROL_WAIT`).
pub(crate) async fn send_reliably(dc: &Arc<dyn DataChannel>, text: &str) {
    let until = Instant::now() + CONTROL_WAIT;
    while dc.send_text(text).await.is_err() && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Canal de dados dos pedaços.
const PIECES_LABEL: &str = "aj-p";
/// Folga de pedaços extras no caminho direto host → espectador (o RTP tem retransmissão;
/// os pedaços não, a folga faz esse papel). Keyframes: o dobro (ver `pieces::overhead_for`).
pub const PIECE_OVERHEAD: f32 = 0.25;
/// Vídeo em pedaços pelo canal de dados em vez de RTP (malha, etapa 1). Desligado por
/// padrão; a sessão liga com AJ_PIECES=1.
pub static USE_PIECES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn runtime() -> Arc<dyn Runtime> {
    static RT: OnceLock<Arc<dyn Runtime>> = OnceLock::new();
    RT.get_or_init(|| default_runtime().expect("runtime tokio do webrtc")).clone()
}

/// Runtime tokio de vida longa: as tarefas que mantêm as conexões vivas rodam aqui.
/// (O `block_on` do webrtc-rs cria um runtime descartável, que mataria essas tarefas.)
pub fn tokio_rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("aj-net")
            .enable_all()
            .build()
            .expect("runtime tokio")
    })
}

/// Executa um future no runtime de rede e espera o resultado (chamar fora de contexto async).
pub fn block_on<F: std::future::Future<Output = T>, T>(fut: F) -> T {
    tokio_rt().block_on(fut)
}

/// Dispara uma tarefa no runtime de rede (pode ser chamado de qualquer thread).
pub fn spawn<F: std::future::Future<Output = ()> + Send + 'static>(fut: F) {
    tokio_rt().spawn(fut);
}

fn video_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90000,
        channels: 0,
        // High profile, nível 5.1 (até 4K60). Os dois lados são o AntiJanja.
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640033".to_owned(),
        rtcp_feedback: vec![
            RTCPFeedback { typ: "nack".into(), parameter: "".into() },
            RTCPFeedback { typ: "nack".into(), parameter: "pli".into() },
        ],
    }
}

fn audio_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1;sprop-stereo=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

/// Onde as conexões escutam: todas as placas de rede, em IPv4 e em IPv6. Com IPv6 (muitas
/// operadoras já dão) não há roteador traduzindo endereço no caminho, e a conexão direta
/// funciona onde o IPv4 fica preso atrás de NAT (CGNAT, NAT simétrico). O ICE testa os
/// dois e fica com o que conectar.
pub(crate) const UDP_BIND: [&str; 2] = ["0.0.0.0:0", "[::]:0"];

/// Oferta/resposta com o endereço aberto no roteador (UPnP) como mais um candidato, se
/// houver (`upnp::augment_sdp`). Conversar com o roteador bloqueia: fora do runtime.
pub(crate) async fn with_upnp(sdp: String) -> String {
    let fallback = sdp.clone();
    tokio::task::spawn_blocking(move || crate::upnp::augment_sdp(&sdp)).await.unwrap_or(fallback)
}

/// Monta a PeerConnection com nossos codecs e STUN.
pub(crate) async fn build_pc(handler: Arc<dyn PeerConnectionEventHandler>, stun: bool) -> Res<Arc<dyn PeerConnection>> {
    let mut me = MediaEngine::default();
    me.register_codec(
        RTCRtpCodecParameters { rtp_codec: video_codec(), payload_type: H264_PT, ..Default::default() },
        RtpCodecKind::Video,
    )
    .map_err(err)?;
    me.register_codec(
        RTCRtpCodecParameters { rtp_codec: audio_codec(), payload_type: OPUS_PT, ..Default::default() },
        RtpCodecKind::Audio,
    )
    .map_err(err)?;
    let registry = register_default_interceptors(Registry::new(), &mut me).map_err(err)?;
    let ice_servers = if stun {
        vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".into(), "stun:stun.cloudflare.com:3478".into()],
            ..Default::default()
        }]
    } else {
        vec![]
    };
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().with_ice_servers(ice_servers).build())
        .with_media_engine(me)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_runtime(runtime())
        .with_udp_addrs(UDP_BIND.to_vec())
        .build()
        .await
        .map_err(err)?;
    Ok(Arc::new(pc))
}

/// Eventos que a camada de cima recebe.
#[derive(Clone, Debug, PartialEq)]
pub enum PeerEvent {
    Connected,
    Disconnected,
    Failed,
    Closed,
}

pub(crate) struct Handler {
    pub(crate) gathered: Sender<()>,
    pub(crate) on_state: Arc<dyn Fn(PeerEvent) + Send + Sync>,
    pub(crate) on_track: Option<Arc<dyn Fn(Arc<dyn TrackRemote>) + Send + Sync>>,
    pub(crate) on_dc: Option<Arc<dyn Fn(Arc<dyn DataChannel>) + Send + Sync>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let ev = match state {
            RTCPeerConnectionState::Connected => PeerEvent::Connected,
            RTCPeerConnectionState::Disconnected => PeerEvent::Disconnected,
            RTCPeerConnectionState::Failed => PeerEvent::Failed,
            RTCPeerConnectionState::Closed => PeerEvent::Closed,
            _ => return,
        };
        (self.on_state)(ev);
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        if let Some(f) = &self.on_track {
            f(track);
        }
    }
    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        if let Some(f) = &self.on_dc {
            f(dc);
        }
    }
}

pub(crate) async fn wait_gathering(rx: &mut webrtc::runtime::Receiver<()>) {
    let timeout = runtime().sleep(ICE_TIMEOUT);
    futures::pin_mut!(timeout);
    let recv = rx.recv();
    futures::pin_mut!(recv);
    let _ = futures::future::select(recv, timeout).await;
}

fn media_track(kind: RtpCodecKind, codec: RTCRtpCodec) -> Res<(MediaStreamTrack, u32)> {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).map_err(err)?;
    let ssrc = u32::from_le_bytes(b);
    let track = MediaStreamTrack::new(
        "antijanja".into(),
        format!("aj-{kind}"),
        format!("aj-{kind}"),
        kind,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
            codec,
            ..Default::default()
        }],
    );
    Ok((track, ssrc))
}

fn new_track(kind: RtpCodecKind, codec: RTCRtpCodec) -> Res<(Arc<TrackLocalStaticSample>, u32)> {
    let (track, ssrc) = media_track(kind, codec)?;
    Ok((Arc::new(TrackLocalStaticSample::new(Instant::now(), track).map_err(err)?), ssrc))
}

/// Solta os pacotes de vídeo no ritmo `rate` (b/s), em vez de todos de uma vez: um
/// keyframe inteiro em rajada enche a fila de roteadores e vira perda.
async fn pace(video: Arc<TrackLocalStaticRTP>, mut rx: tokio::sync::mpsc::Receiver<Vec<rtc::rtp::Packet>>, rate: Arc<AtomicU64>) {
    let mut next = tokio::time::Instant::now();
    while let Some(frame) = rx.recv().await {
        for pkt in frame {
            let now = tokio::time::Instant::now();
            if next + PACER_BURST < now {
                next = now - PACER_BURST;
            }
            if next > now {
                tokio::time::sleep_until(next).await;
            }
            let bits = ((pkt.payload.len() + 40) * 8) as f64; // + cabeçalhos IP/UDP/RTP
            next += Duration::from_secs_f64(bits / rate.load(Ordering::Relaxed).max(1) as f64);
            // Erro (ex.: faixa ainda não ligada à conexão): perde só este pacote. O pacer
            // termina quando o HostPeer some (o canal fecha).
            let _ = video.write_rtp(pkt).await;
        }
    }
}

/// Como `pace`, para os pedaços. Cada mensagem leva na frente um número sequencial do
/// canal (4 bytes): quem recebe mede a perda por ele.
/// O áudio (`audio_rx`) passa na frente: sai na hora, até no meio de um quadro de vídeo
/// sendo espaçado (um keyframe pode levar dezenas de ms; o áudio não espera).
async fn pace_pieces(
    slot: Arc<Mutex<Option<Arc<dyn DataChannel>>>>,
    mut rx: tokio::sync::mpsc::Receiver<Vec<Vec<u8>>>,
    mut audio_rx: tokio::sync::mpsc::Receiver<Vec<Vec<u8>>>,
    rate: Arc<AtomicU64>,
) {
    let mut next = tokio::time::Instant::now();
    let mut seq: u32 = 0;
    async fn send(dc: &Arc<dyn DataChannel>, seq: &mut u32, piece: &[u8]) {
        let mut b = BytesMut::with_capacity(4 + piece.len());
        b.extend_from_slice(&seq.to_le_bytes());
        b.extend_from_slice(piece);
        *seq = seq.wrapping_add(1);
        let _ = dc.send(b).await;
    }
    loop {
        let frame = tokio::select! {
            biased;
            a = audio_rx.recv() => match a {
                Some(a) => {
                    let dc = slot.lock().unwrap().clone();
                    if let Some(dc) = dc {
                        for p in a {
                            send(&dc, &mut seq, &p).await;
                        }
                    }
                    continue;
                }
                None => break,
            },
            f = rx.recv() => match f {
                Some(f) => f,
                None => break,
            },
        };
        let dc = slot.lock().unwrap().clone();
        let Some(dc) = dc else { continue };
        for piece in frame {
            while let Ok(a) = audio_rx.try_recv() {
                for p in a {
                    send(&dc, &mut seq, &p).await;
                }
            }
            let now = tokio::time::Instant::now();
            if next + PACER_BURST < now {
                next = now - PACER_BURST;
            }
            if next > now {
                tokio::time::sleep_until(next).await;
            }
            let bits = ((piece.len() + 4 + 60) * 8) as f64; // + cabeçalhos SCTP/DTLS/UDP/IP
            next += Duration::from_secs_f64(bits / rate.load(Ordering::Relaxed).max(1) as f64);
            send(&dc, &mut seq, &piece).await;
        }
    }
}

/* ========================================================================== */
/*                                   HOST                                      */
/* ========================================================================== */

/// Um espectador conectado ao host.
pub struct HostPeer {
    pub pc: Arc<dyn PeerConnection>,
    /// Quebra cada quadro H.264 em pacotes RTP (o envio fica com o pacer).
    packetizer: Mutex<Box<dyn Packetizer>>,
    pacer: tokio::sync::mpsc::Sender<Vec<rtc::rtp::Packet>>,
    pacing_bps: Arc<AtomicU64>,
    audio: Arc<TrackLocalStaticSample>,
    assrc: u32,
    pts: Mutex<Option<(PayloadType, PayloadType)>>,
    senders: Vec<Arc<dyn webrtc::rtp_transceiver::RtpSender>>,
    pub dc: Mutex<Option<Arc<dyn DataChannel>>>,
    /// Canal dos pedaços (o espectador cria; None = versão antiga, só RTP).
    pieces_dc: Arc<Mutex<Option<Arc<dyn DataChannel>>>>,
    piece_tx: tokio::sync::mpsc::Sender<Vec<Vec<u8>>>,
    audio_tx: tokio::sync::mpsc::Sender<Vec<Vec<u8>>>,
    /// Faixas que o host manda direto para este espectador (bit `l` = faixa `l`). As
    /// outras ele recebe de outros espectadores (malha). Todas, no começo.
    direct_lanes: std::sync::atomic::AtomicU32,
    /// Pedaços entregues ao pacer (diagnóstico e testes: upload do host).
    pub pieces_sent: AtomicU64,
}

impl HostPeer {
    /// Responde à oferta de um espectador. `on_keyframe` é chamado quando ele pede keyframe.
    pub async fn answer(
        offer_sdp: String,
        stun: bool,
        on_state: Arc<dyn Fn(PeerEvent) + Send + Sync>,
        on_message: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Res<(Arc<HostPeer>, String)> {
        let (gtx, mut grx) = channel::<()>(1);
        let dc_slot: Arc<Mutex<Option<Arc<dyn DataChannel>>>> = Arc::new(Mutex::new(None));
        let pieces_slot: Arc<Mutex<Option<Arc<dyn DataChannel>>>> = Arc::new(Mutex::new(None));
        let (slot, pslot) = (dc_slot.clone(), pieces_slot.clone());
        let on_dc: Arc<dyn Fn(Arc<dyn DataChannel>) + Send + Sync> = Arc::new(move |dc: Arc<dyn DataChannel>| {
            let on_message = on_message.clone();
            let (slot, pslot) = (slot.clone(), pslot.clone());
            spawn(async move {
                if dc.label().await.ok().as_deref() == Some(PIECES_LABEL) {
                    *pslot.lock().unwrap() = Some(dc.clone());
                    // Nada chega por ele no host; só esvazia os eventos.
                    while let Some(ev) = dc.poll().await {
                        if matches!(ev, DataChannelEvent::OnClose) {
                            break;
                        }
                    }
                    return;
                }
                *slot.lock().unwrap() = Some(dc.clone());
                while let Some(ev) = dc.poll().await {
                    match ev {
                        DataChannelEvent::OnMessage(m) => on_message(String::from_utf8_lossy(&m.data).into_owned()),
                        DataChannelEvent::OnClose => break,
                        _ => {}
                    }
                }
            });
        });
        let handler = Arc::new(Handler { gathered: gtx, on_state, on_track: None, on_dc: Some(on_dc) });
        let pc = build_pc(handler, stun).await?;

        let (video_track, vssrc) = media_track(RtpCodecKind::Video, video_codec())?;
        let video = Arc::new(TrackLocalStaticRTP::new(video_track));
        let (audio, assrc) = new_track(RtpCodecKind::Audio, audio_codec())?;
        let vs = pc.add_track(video.clone() as Arc<dyn TrackLocal>).await.map_err(err)?;
        let a_s = pc.add_track(audio.clone() as Arc<dyn TrackLocal>).await.map_err(err)?;

        pc.set_remote_description(RTCSessionDescription::offer(offer_sdp).map_err(err)?).await.map_err(err)?;
        let answer = pc.create_answer(None).await.map_err(err)?;
        pc.set_local_description(answer).await.map_err(err)?;
        wait_gathering(&mut grx).await;
        let sdp = with_upnp(pc.local_description().await.ok_or("sem descrição local")?.sdp).await;

        let packetizer: Box<dyn Packetizer> = Box::new(rtc::rtp::packetizer::new_packetizer(
            Instant::now(),
            RTP_MTU,
            0, // o payload type negociado vai em cada pacote
            vssrc,
            video_codec().payloader().map_err(err)?,
            Box::new(rtc::rtp::sequence::new_random_sequencer()),
            90000,
        ));
        let (pacer, pacer_rx) = tokio::sync::mpsc::channel(PACER_QUEUE);
        // Até saber o alvo (logo que o espectador entra), solta com folga.
        let pacing_bps = Arc::new(AtomicU64::new(40_000_000));
        spawn(pace(video.clone(), pacer_rx, pacing_bps.clone()));
        let (piece_tx, piece_rx) = tokio::sync::mpsc::channel(PACER_QUEUE);
        let (audio_tx, audio_rx) = tokio::sync::mpsc::channel(250);
        spawn(pace_pieces(pieces_slot.clone(), piece_rx, audio_rx, pacing_bps.clone()));
        let peer = Arc::new(HostPeer {
            pc, audio, assrc,
            packetizer: Mutex::new(packetizer),
            pacer,
            pacing_bps,
            pts: Mutex::new(None),
            senders: vec![vs, a_s],
            dc: Mutex::new(None),
            pieces_dc: pieces_slot,
            piece_tx,
            audio_tx,
            direct_lanes: std::sync::atomic::AtomicU32::new(u32::MAX),
            pieces_sent: AtomicU64::new(0),
        });
        // O canal de dados chega depois; guardamos quando aparecer.
        let p = peer.clone();
        spawn(async move {
            for _ in 0..200 {
                if let Some(dc) = dc_slot.lock().unwrap().clone() {
                    *p.dc.lock().unwrap() = Some(dc);
                    return;
                }
                runtime().sleep(Duration::from_millis(100)).await;
            }
        });
        Ok((peer, sdp))
    }

    async fn payload_types(&self) -> Option<(PayloadType, PayloadType)> {
        if let Some(p) = *self.pts.lock().unwrap() {
            return Some(p);
        }
        let mut found = Vec::new();
        for s in &self.senders {
            let pt = s.get_parameters().await.ok()?.rtp_parameters.codecs.first()?.payload_type;
            found.push(pt);
        }
        let pts = (found[0], found[1]);
        *self.pts.lock().unwrap() = Some(pts);
        Some(pts)
    }

    /// Entrega um quadro ao pacer. `Ok(false)`: a fila do pacer está cheia e o quadro foi
    /// descartado (o espectador vai precisar de um keyframe).
    pub async fn send_video(&self, data: Bytes, duration: Duration) -> Res<bool> {
        let (vpt, _) = self.payload_types().await.ok_or("sem codec negociado")?;
        let samples = (duration.as_secs_f64() * 90000.0).round() as u32;
        let mut packetizer = self.packetizer.lock().unwrap();
        if self.pacer.capacity() == 0 {
            // Descarta antes de numerar os pacotes (senão o espectador veria um buraco
            // e pediria retransmissão de algo que nunca saiu), mas avança o relógio RTP.
            packetizer.skip_samples(samples);
            return Ok(false);
        }
        let mut packets = packetizer.packetize(Instant::now(), &data, samples).map_err(err)?;
        drop(packetizer);
        for p in &mut packets {
            p.header.payload_type = vpt;
        }
        Ok(self.pacer.try_send(packets).is_ok())
    }

    /// O espectador recebe pedaços (tem o canal "aj-p")?
    pub fn has_pieces(&self) -> bool {
        self.pieces_dc.lock().unwrap().is_some()
    }

    /// Manda os pedaços das faixas que vão direto para este espectador. `false`: a fila
    /// está cheia e o quadro ficou para trás (o espectador vai pedir keyframe).
    pub fn send_pieces(&self, frame: &crate::pieces::PieceFrame) -> bool {
        let mask = self.direct_lanes.load(Ordering::Relaxed);
        if frame.audio {
            let pieces: Vec<Vec<u8>> =
                frame.pieces.iter().filter(|p| crate::pieces::lane_of(p).is_some_and(|l| mask & (1 << l) != 0)).cloned().collect();
            return pieces.is_empty() || self.audio_tx.try_send(pieces).is_ok();
        }
        if self.piece_tx.capacity() == 0 {
            return false;
        }
        let pieces: Vec<Vec<u8>> = frame
            .pieces
            .iter()
            .filter(|p| crate::pieces::lane_of(p).is_some_and(|l| mask & (1 << l) != 0))
            .cloned()
            .collect();
        let n = pieces.len() as u64;
        let ok = pieces.is_empty() || self.piece_tx.try_send(pieces).is_ok();
        if ok {
            self.pieces_sent.fetch_add(n, Ordering::Relaxed);
        }
        ok
    }

    /// Pedaços extras pedidos por este espectador (reparo), sem filtro de faixa.
    pub fn send_extra(&self, pieces: Vec<Vec<u8>>) {
        if !pieces.is_empty() && self.piece_tx.try_send(pieces).is_ok() {
            REPAIRS_SENT.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Faixas que o host manda direto (bit `l` = faixa `l`).
    pub fn set_direct_lanes(&self, mask: u32) {
        self.direct_lanes.store(mask, Ordering::Relaxed);
    }

    /// Bitrate alvo atual do vídeo (b/s): o pacer solta os pacotes com folga sobre ele.
    pub fn set_target_bitrate(&self, bps: u32) {
        let pacing = ((bps as f64 * PACING_FACTOR) as u64).max(MIN_PACING_BPS);
        self.pacing_bps.store(pacing, Ordering::Relaxed);
    }

    pub async fn send_audio(&self, data: Bytes, duration: Duration) -> Res<()> {
        let (_, apt) = self.payload_types().await.ok_or("sem codec negociado")?;
        self.audio
            .sample_writer(self.assrc, apt)
            .write_sample(&Sample { data, duration, ..Sample::new(Instant::now()) })
            .await
            .map_err(err)
    }

    /// Manda pelo canal "aj". Logo depois de conectar o canal pode ainda não ter chegado
    /// ou aberto: espera até 5 s em vez de perder a mensagem (as da malha não podem se
    /// perder: uma oferta perdida deixava a ligação entre espectadores sem nascer).
    pub async fn send_message(&self, text: &str) {
        let until = Instant::now() + CONTROL_WAIT;
        loop {
            let dc = self.dc.lock().unwrap().clone();
            if let Some(dc) = dc {
                if dc.send_text(text).await.is_ok() {
                    return;
                }
            }
            if Instant::now() >= until {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/* ========================================================================== */
/*                                 ESPECTADOR                                  */
/* ========================================================================== */

pub struct ViewerSession {
    pub pc: Arc<dyn PeerConnection>,
    dc: Arc<dyn DataChannel>,
    pub mesh: Arc<crate::meshnet::ViewerMesh>,
}

/// Chave pública com que o host assina os quadros (malha). Chega pela conexão direta com
/// o host (cifrada e autenticada pela troca feita com o código), antes de qualquer rota.
type StreamKey = Arc<Mutex<Option<ed25519_dalek::VerifyingKey>>>;

/// Callbacks de mídia do espectador.
#[derive(Clone)]
pub struct ViewerSinks {
    /// Quadro H.264 completo (Annex B) + timestamp RTP (90 kHz).
    pub on_video: Arc<dyn Fn(Vec<u8>, u32) + Send + Sync>,
    /// Pacote Opus.
    pub on_audio: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
    pub on_state: Arc<dyn Fn(PeerEvent) + Send + Sync>,
    pub on_message: Arc<dyn Fn(String) + Send + Sync>,
    /// Pedaços (vídeo e áudio com o horário de captura no relógio comum, para tocar
    /// sincronizados). None: o vídeo dos pedaços vai para `on_video` e o áudio é ignorado.
    pub timed: Option<Arc<dyn Fn(Timed) + Send + Sync>>,
}

/// O que chega pelos pedaços, com o horário de captura (90 kHz, relógio comum do host).
pub enum Timed {
    /// Quadro H.264 completo e conferido, na ordem.
    Video(Vec<u8>, u32),
    /// Pacote Opus: (número, horário, dados). Pode vir repetido e fora de ordem.
    Audio(u32, u32, Vec<u8>),
}

impl ViewerSession {
    /// Cria a oferta (só recebe vídeo e áudio).
    pub async fn offer(sinks: ViewerSinks, stun: bool) -> Res<(Arc<ViewerSession>, String)> {
        let (gtx, mut grx) = channel::<()>(1);
        let dc_cell: Arc<OnceLock<Arc<dyn DataChannel>>> = Arc::new(OnceLock::new());
        let cell = dc_cell.clone();
        let s = sinks.clone();
        let on_track: Arc<dyn Fn(Arc<dyn TrackRemote>) + Send + Sync> = Arc::new(move |track: Arc<dyn TrackRemote>| {
            let s = s.clone();
            let cell = cell.clone();
            spawn(async move { receive_track(track, s, cell).await });
        });
        let handler = Arc::new(Handler { gathered: gtx, on_state: sinks.on_state.clone(), on_track: Some(on_track), on_dc: None });
        let pc = build_pc(handler, stun).await?;
        for kind in [RtpCodecKind::Video, RtpCodecKind::Audio] {
            pc.add_transceiver_from_kind(
                kind,
                Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, ..Default::default() }),
            )
            .await
            .map_err(err)?;
        }
        let dc = pc.create_data_channel("aj", None).await.map_err(err)?;
        let _ = dc_cell.set(dc.clone());
        // Pedaços: fora de ordem e sem reenvio (a folga dos pedaços extras cobre a perda).
        let init = webrtc::data_channel::RTCDataChannelInit { ordered: false, max_retransmits: Some(0), ..Default::default() };
        let pdc = pc.create_data_channel(PIECES_LABEL, Some(init)).await.map_err(err)?;
        // Pedaços do host (fonte 0) e dos vizinhos da malha (fonte = id) num receptor só.
        let (ptx, prx) = tokio::sync::mpsc::channel::<(u64, Vec<u8>)>(4096);
        let mesh = crate::meshnet::ViewerMesh::new(stun, dc_cell.clone(), ptx.clone());
        let stream_key: StreamKey = Arc::new(Mutex::new(None));
        {
            let tx = ptx.clone();
            spawn(async move {
                while let Some(ev) = pdc.poll().await {
                    match ev {
                        DataChannelEvent::OnMessage(m) => {
                            let _ = tx.try_send((0, m.data.to_vec()));
                        }
                        DataChannelEvent::OnClose => break,
                        _ => {}
                    }
                }
            });
            let (s, cell, m, k) = (sinks.clone(), dc_cell.clone(), mesh.clone(), stream_key.clone());
            spawn(async move { receive_pieces(prx, s, cell, m, k).await });
        }
        let on_message = sinks.on_message.clone();
        let dcc = dc.clone();
        let (m, key) = (mesh.clone(), stream_key.clone());
        spawn(async move {
            while let Some(ev) = dcc.poll().await {
                match ev {
                    DataChannelEvent::OnMessage(msg) => {
                        let text = String::from_utf8_lossy(&msg.data).into_owned();
                        // Rotas, oferta/resposta e a chave da malha ficam aqui; o resto sobe.
                        let v = serde_json::from_str::<serde_json::Value>(&text).ok();
                        if let Some(pk) = v.as_ref().filter(|v| v["t"] == "stream-key").and_then(|v| v["pk"].as_str()) {
                            let bytes: Option<[u8; 32]> = (0..32).map(|i| pk.get(i * 2..i * 2 + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect::<Option<Vec<u8>>>().and_then(|b| b.try_into().ok());
                            if let Some(k) = bytes.and_then(|b| ed25519_dalek::VerifyingKey::from_bytes(&b).ok()) {
                                *key.lock().unwrap() = Some(k);
                            }
                            continue;
                        }
                        let mine = v.is_some_and(|v| m.on_host_message(&v));
                        if !mine {
                            on_message(text);
                        }
                    }
                    DataChannelEvent::OnClose => break,
                    _ => {}
                }
            }
            m.close();
        });

        let offer = pc.create_offer(None).await.map_err(err)?;
        pc.set_local_description(offer).await.map_err(err)?;
        wait_gathering(&mut grx).await;
        let sdp = with_upnp(pc.local_description().await.ok_or("sem descrição local")?.sdp).await;
        Ok((Arc::new(ViewerSession { pc, dc, mesh }), sdp))
    }

    pub async fn accept_answer(&self, sdp: String) -> Res<()> {
        self.pc.set_remote_description(RTCSessionDescription::answer(sdp).map_err(err)?).await.map_err(err)
    }

    pub async fn send_message(&self, text: &str) {
        send_reliably(&self.dc, text).await;
    }

    /// Tempo de ida e volta até o host (ms), do par ICE em uso.
    pub async fn rtt_ms(&self) -> Option<f64> {
        let report = self.pc.get_stats(Instant::now(), webrtc::peer_connection::StatsSelector::None).await;
        let rtt = report
            .candidate_pairs()
            .filter(|p| p.nominated && p.current_round_trip_time > 0.0)
            .map(|p| p.current_round_trip_time * 1000.0)
            .next();
        rtt
    }
}

/// Lê RTP de uma faixa remota. Vídeo: remonta quadros H.264 e pede keyframe se perder pacotes.
async fn receive_track(track: Arc<dyn TrackRemote>, sinks: ViewerSinks, dc: Arc<OnceLock<Arc<dyn DataChannel>>>) {
    let kind = track.kind().await;
    let ssrc = track.ssrcs().await.first().copied().unwrap_or(0);
    if kind == RtpCodecKind::Audio {
        while let Some(ev) = track.poll().await {
            match ev {
                TrackRemoteEvent::OnRtpPacket(p) => (sinks.on_audio)(p.payload.to_vec()),
                TrackRemoteEvent::OnEnded => break,
                _ => {}
            }
        }
        return;
    }

    let request_keyframe = |track: &Arc<dyn TrackRemote>| {
        let track = track.clone();
        let dc = dc.get().cloned();
        async move {
            KEYFRAME_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = track.write_rtcp(vec![Box::new(PictureLossIndication { sender_ssrc: 0, media_ssrc: ssrc })]).await;
            if let Some(dc) = dc {
                let _ = dc.send_text(r#"{"t":"kf"}"#).await;
            }
        }
    };

    let mut jb = JitterBuffer::new();
    let mut asm = FrameAssembler::new();
    let mut meter = NetMeter::new(Instant::now());
    let mut last_request = Instant::now() - Duration::from_secs(10);

    // Simulação de rede (só em testes): segura um pacote e solta depois do próximo.
    let mut held: Option<rtc::rtp::Packet> = None;
    let mut rng: u32 = 0x9e37_79b9;

    loop {
        // Com lacuna pendente, espera no máximo até o prazo de desistir dela.
        let wait = jb.deadline().map(|d| d.saturating_duration_since(Instant::now()));
        let ev = match wait {
            Some(w) => match tokio::time::timeout(w, track.poll()).await {
                Ok(ev) => ev,
                Err(_) => {
                    // Prazo estourado: o pacote não veio nem por retransmissão.
                    for p in jb.give_up() {
                        asm.push(&p, &sinks);
                    }
                    asm.mark_broken();
                    continue;
                }
            },
            None => track.poll().await,
        };
        let p = match ev {
            Some(TrackRemoteEvent::OnRtpPacket(p)) => p,
            Some(TrackRemoteEvent::OnEnded) | None => break,
            Some(_) => continue,
        };

        meter.on_packet(Instant::now(), p.payload.len(), p.header.timestamp);
        let permille = TEST_REORDER_PERMILLE.load(std::sync::atomic::Ordering::Relaxed);
        let mut batch = Vec::with_capacity(2);
        if permille > 0 {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            if let Some(h) = held.take() {
                batch.push(p);
                batch.push(h);
            } else if rng % 1000 < permille {
                held = Some(p);
            } else {
                batch.push(p);
            }
        } else {
            batch.push(p);
        }

        for p in batch {
            for ready in jb.push(p) {
                asm.push(&ready, &sinks);
            }
        }
        if asm.needs_keyframe() && last_request.elapsed() > KEYFRAME_REQUEST_INTERVAL {
            last_request = Instant::now();
            request_keyframe(&track).await;
        }
        // Relatório para o bitrate adaptativo do host.
        if let Some(r) = meter.report(Instant::now(), jb.received, jb.last_ext) {
            if let (Some(dc), Ok(json)) = (dc.get(), serde_json::to_value(r)) {
                let mut msg = json;
                msg["t"] = "rr".into();
                let _ = dc.send_text(&msg.to_string()).await;
            }
        }
    }
}

/// Recebe o vídeo em pedaços (canal "aj-p"): monta os quadros, entrega na ordem, pede
/// keyframe quando perde um e manda os relatórios para o bitrate adaptativo do host.
async fn receive_pieces(
    mut rx: tokio::sync::mpsc::Receiver<(u64, Vec<u8>)>,
    sinks: ViewerSinks,
    dc: Arc<OnceLock<Arc<dyn DataChannel>>>,
    mesh: Arc<crate::meshnet::ViewerMesh>,
    stream_key: StreamKey,
) {
    use crate::pieces::{Assembler, Header, Ordered, Verifier, KIND_AUDIO, KIND_SIG, KIND_VIDEO};
    let mut asm = Assembler::new();
    // Com a chave do host conhecida (malha), só passa quadro com assinatura dele.
    let mut verifier: Option<Verifier> = None;
    let mut ord = Ordered::new();
    // Versão do vídeo que está chegando: a troca começa num keyframe da nova, com a
    // numeração dela (cada versão conta os próprios quadros).
    let mut rendition: Option<u16> = None;
    let mut meter = NetMeter::new(Instant::now());
    let mut last_request = Instant::now() - Duration::from_secs(10);
    // Perda pelo número sequencial do canal (estendido além dos 32 bits).
    let (mut received, mut last_seq, mut ext) = (0u64, None::<u32>, 0i64);
    let mut highest: Option<u64> = None;
    let mut rng: u32 = 0x2545_f491;
    // Pedidos de reparo por quadro: (versão, quadro, quantas vezes, quando foi o último).
    let mut repair_asked: std::collections::VecDeque<(u16, u32, u8, Instant)> = Default::default();
    loop {
        // Sem pedaços chegando, ainda acorda para dar quadros atrasados como perdidos.
        let ev = match tokio::time::timeout(Duration::from_millis(20), rx.recv()).await {
            Ok(Some(ev)) => Some(ev),
            Ok(None) => break,
            Err(_) => None,
        };
        let now = Instant::now();
        let mut out = vec![];
        let mut verified: Vec<crate::pieces::Frame> = vec![];
        match ev {
            // Pedaço repassado por um vizinho: o número sequencial é da ligação dele (a
            // perda medida para o bitrate do host é só a do caminho direto).
            Some((src, d)) if src != 0 && d.len() > 4 => {
                let piece = &d[4..];
                mesh.on_piece(src, piece, now).await;
                // Áudio de vizinho: só com a assinatura do host conferida.
                if Header::read(piece).is_some_and(|h| h.kind == KIND_AUDIO) {
                    let key = *stream_key.lock().unwrap();
                    if let (Some(k), Some(t)) = (key, sinks.timed.as_ref()) {
                        if let Some((seq, ts, opus)) = crate::pieces::parse_audio(piece, Some(&k)) {
                            t(Timed::Audio(seq, ts, opus));
                        }
                    }
                    continue;
                }
                if verifier.is_none() {
                    // Pedaços de vizinho sem a chave para conferir: não confia.
                    if let Some(k) = *stream_key.lock().unwrap() {
                        verifier = Some(Verifier::new(k));
                        asm.keep_raw = true;
                    }
                }
                if let Some(v) = verifier.as_mut() {
                    if Header::read(piece).is_some_and(|h| h.kind == KIND_SIG) {
                        verified.extend(v.on_sig(piece, now));
                    } else if let Some(f) = asm.push_from(piece, now, src) {
                        verified.extend(v.push(f, now));
                    }
                }
            }
            Some((_, d)) if d.len() > 4 => {
                let loss = TEST_PIECE_LOSS_PERMILLE.load(Ordering::Relaxed);
                if loss > 0 {
                    rng ^= rng << 13;
                    rng ^= rng >> 17;
                    rng ^= rng << 5;
                    if rng % 1000 < loss {
                        continue; // perdido na "rede"
                    }
                }
                let seq = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
                if let Some(last) = last_seq {
                    ext += seq.wrapping_sub(last) as i32 as i64;
                }
                last_seq = Some(seq);
                received += 1;
                highest = Some(highest.map_or(ext.max(0) as u64, |h| h.max(ext.max(0) as u64)));
                let piece = &d[4..];
                if let Some(h) = Header::read(piece) {
                    meter.on_packet(now, d.len(), h.ts);
                }
                mesh.on_piece(0, piece, now).await;
                // Áudio direto do host: confere a assinatura se a chave é conhecida.
                if Header::read(piece).is_some_and(|h| h.kind == KIND_AUDIO) {
                    let key = *stream_key.lock().unwrap();
                    if let Some(t) = sinks.timed.as_ref() {
                        if let Some((seq, ts, opus)) = crate::pieces::parse_audio(piece, key.as_ref()) {
                            t(Timed::Audio(seq, ts, opus));
                        }
                    }
                    continue;
                }
                if verifier.is_none() {
                    if let Some(k) = *stream_key.lock().unwrap() {
                        verifier = Some(Verifier::new(k));
                        asm.keep_raw = true;
                    }
                }
                let is_sig = Header::read(piece).is_some_and(|h| h.kind == KIND_SIG);
                match verifier.as_mut() {
                    Some(v) if is_sig => verified.extend(v.on_sig(piece, now)),
                    Some(v) => {
                        if let Some(f) = asm.push(piece, now) {
                            verified.extend(v.push(f, now));
                        }
                    }
                    // Sem malha (só o host manda, pela conexão direta): sem assinatura.
                    None => {
                        if let Some(f) = asm.push(piece, now) {
                            verified.push(f);
                        }
                    }
                }
            }
            _ => {}
        }
        if let Some(v) = verifier.as_mut() {
            verified.extend(v.tick(now));
            verified.extend(v.retry(&asm, now));
            if !v.suspects.is_empty() {
                let s = std::mem::take(&mut v.suspects);
                mesh.report_forged(&s);
            }
        }
        for f in verified.drain(..) {
            if f.kind != KIND_VIDEO {
                continue;
            }
            if rendition != Some(f.rendition) && f.keyframe {
                rendition = Some(f.rendition);
                ord = Ordered::new();
            }
            if rendition == Some(f.rendition) {
                out.extend(ord.push(f, now));
            }
        }
        mesh.tick(now);
        out.extend(ord.tick(now));
        // Um quadro está faltando e os seguintes esperam: se ele chegou em parte, pede ao
        // host os pedaços que faltam (pedaços novos: o host não precisa saber quais se
        // perderam). Chega em um ida e volta, antes de o quadro ser dado como perdido.
        // Os pedaços do reparo também podem se perder: pede com folga e tenta de novo
        // (até 3 vezes, a cada 25 ms) enquanto o quadro ainda pode ser salvo.
        if let (Some(n), Some(r)) = (ord.waiting_for(), rendition) {
            let prev = repair_asked.iter().position(|x| x.0 == r && x.1 == n);
            let due = match prev {
                None => true,
                Some(i) => repair_asked[i].2 < 3 && now.saturating_duration_since(repair_asked[i].3) >= Duration::from_millis(25),
            };
            if due {
                if let Some(blocks) = asm.missing_blocks(KIND_VIDEO, r, n) {
                    match prev {
                        Some(i) => {
                            repair_asked[i].2 += 1;
                            repair_asked[i].3 = now;
                        }
                        None => repair_asked.push_back((r, n, 1, now)),
                    }
                    while repair_asked.len() > 64 {
                        repair_asked.pop_front();
                    }
                    let b: Vec<[u32; 2]> = blocks.iter().map(|&(b, k)| [b as u32, k + 2 + (k * 3).div_ceil(10)]).collect();
                    if let Some(dc) = dc.get() {
                        let _ = dc.send_text(&serde_json::json!({ "t": "more", "r": r, "f": n, "b": b }).to_string()).await;
                    }
                }
            }
        }
        for f in out {
            match sinks.timed.as_ref() {
                Some(t) => t(Timed::Video(f.data, f.ts)),
                None => (sinks.on_video)(f.data, f.ts),
            }
        }
        // Só depois de receber algum pedaço (com o host no RTP, este canal fica mudo).
        if received > 0 && ord.needs_keyframe() && last_request.elapsed() > KEYFRAME_REQUEST_INTERVAL {
            last_request = Instant::now();
            KEYFRAME_REQUESTS.fetch_add(1, Ordering::Relaxed);
            if let Some(dc) = dc.get() {
                let _ = dc.send_text(r#"{"t":"kf"}"#).await;
            }
        }
        if let Some(r) = meter.report(now, received, highest) {
            if let (Some(dc), Ok(mut msg)) = (dc.get(), serde_json::to_value(r)) {
                msg["t"] = "rr".into();
                let _ = dc.send_text(&msg.to_string()).await;
            }
        }
    }
}

/// Intervalo mínimo entre pedidos de keyframe do espectador. Cada keyframe é ~10x um
/// quadro normal: pedir a cada 300 ms (como antes) triplicava o bitrate justamente quando a
/// rede já estava perdendo pacotes, e piorava a perda.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_secs(1);

/// Medidas do espectador para o bitrate adaptativo do host (ver `congestion`).
struct NetMeter {
    start: Instant,
    window_start: Instant,
    bytes: usize,
    /// (pacotes recebidos, maior número de sequência) no começo da janela.
    at_start: Option<(u64, u64)>,
    last_ts: Option<u32>,
    ext_ts: i64,
    /// Trânsito (chegada − horário RTP, ms) do 1º pacote de cada quadro nesta janela.
    transits: Vec<f64>,
    /// Trânsitos dos últimos 10 s (o mínimo é a referência de "sem fila").
    history: std::collections::VecDeque<(f64, f64)>,
}

impl NetMeter {
    fn new(now: Instant) -> Self {
        Self { start: now, window_start: now, bytes: 0, at_start: None, last_ts: None, ext_ts: 0, transits: vec![], history: Default::default() }
    }

    fn on_packet(&mut self, now: Instant, payload: usize, rtp_ts: u32) {
        self.bytes += payload;
        if self.last_ts == Some(rtp_ts) {
            return;
        }
        // Primeiro pacote de um quadro: o atraso dele não inclui o envio espaçado do
        // próprio quadro (pacer), só a fila que já havia antes.
        if let Some(last) = self.last_ts {
            self.ext_ts += rtp_ts.wrapping_sub(last) as i32 as i64;
        }
        self.last_ts = Some(rtp_ts);
        let local = now.duration_since(self.start).as_secs_f64() * 1000.0;
        let transit = local - self.ext_ts as f64 / 90.0;
        self.transits.push(transit);
        self.history.push_back((local, transit));
        while self.history.front().is_some_and(|&(t, _)| local - t > 10_000.0) {
            self.history.pop_front();
        }
    }

    /// A cada `congestion::REPORT_INTERVAL`: taxa, perda e fila da janela que acabou.
    fn report(&mut self, now: Instant, received: u64, highest: Option<u64>) -> Option<crate::congestion::Report> {
        let highest = highest?;
        let Some((rx0, hi0)) = self.at_start else {
            self.at_start = Some((received, highest));
            self.window_start = now;
            self.bytes = 0;
            self.transits.clear();
            return None;
        };
        let el = now.duration_since(self.window_start);
        if el < crate::congestion::REPORT_INTERVAL {
            return None;
        }
        let expected = highest.saturating_sub(hi0);
        let got = received.saturating_sub(rx0);
        let loss = if expected > 0 { (1.0 - got as f64 / expected as f64).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let min = self.history.iter().map(|&(_, t)| t).fold(f64::INFINITY, f64::min);
        let qd = if self.transits.is_empty() || !min.is_finite() {
            0.0
        } else {
            (self.transits.iter().sum::<f64>() / self.transits.len() as f64 - min).max(0.0) as f32
        };
        let kbps = (self.bytes as f64 * 8.0 / el.as_secs_f64() / 1000.0) as u32;
        self.at_start = Some((received, highest));
        self.window_start = now;
        self.bytes = 0;
        self.transits.clear();
        Some(crate::congestion::Report { kbps, loss, qd })
    }
}

/// Reordena pacotes RTP e espera retransmissões (NACK) antes de desistir de uma lacuna.
struct JitterBuffer {
    buf: std::collections::BTreeMap<u64, rtc::rtp::Packet>,
    next: Option<u64>,
    last_ext: Option<u64>,
    gap_since: Option<Instant>,
    /// Pacotes únicos recebidos (para a perda do relatório).
    received: u64,
}

/// Quanto esperar por um pacote que faltou (a retransmissão leva ~1 RTT + intervalo do NACK).
const MAX_GAP_WAIT: Duration = Duration::from_millis(250);
const MAX_BUFFERED: usize = 2000;

impl JitterBuffer {
    fn new() -> Self {
        Self { buf: Default::default(), next: None, last_ext: None, gap_since: None, received: 0 }
    }

    /// Número de sequência estendido (sem a virada de 16 bits).
    fn extend(&mut self, seq: u16) -> u64 {
        let ext = match self.last_ext {
            None => (1u64 << 32) + seq as u64,
            Some(last) => {
                let base = last & !0xffff;
                let cands = [base + seq as u64, (base + seq as u64).wrapping_add(1 << 16), (base + seq as u64).wrapping_sub(1 << 16)];
                *cands.iter().min_by_key(|&&c| c.abs_diff(last)).unwrap()
            }
        };
        if self.last_ext.is_none_or(|l| ext > l) {
            self.last_ext = Some(ext);
        }
        ext
    }

    /// Recebe um pacote e devolve os que já podem seguir, em ordem.
    fn push(&mut self, p: rtc::rtp::Packet) -> Vec<rtc::rtp::Packet> {
        let ext = self.extend(p.header.sequence_number);
        let next = *self.next.get_or_insert(ext);
        if ext < next {
            return Vec::new(); // atrasado demais ou duplicado
        }
        if self.buf.insert(ext, p).is_none() {
            self.received += 1;
        }
        let out = self.drain();
        if self.buf.len() > MAX_BUFFERED {
            let mut more = self.give_up();
            let mut all = out;
            all.append(&mut more);
            return all;
        }
        out
    }

    fn drain(&mut self) -> Vec<rtc::rtp::Packet> {
        let mut out = Vec::new();
        while let Some(next) = self.next {
            match self.buf.remove(&next) {
                Some(p) => {
                    out.push(p);
                    self.next = Some(next + 1);
                }
                None => break,
            }
        }
        if self.buf.is_empty() {
            self.gap_since = None;
        } else if self.gap_since.is_none() {
            self.gap_since = Some(Instant::now());
        }
        out
    }

    /// Prazo para desistir da lacuna atual (se houver).
    fn deadline(&self) -> Option<Instant> {
        self.gap_since.map(|t| t + MAX_GAP_WAIT)
    }

    /// Desiste do(s) pacote(s) que faltam: pula para o próximo disponível.
    fn give_up(&mut self) -> Vec<rtc::rtp::Packet> {
        BROKEN_FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.next = self.buf.keys().next().copied();
        self.gap_since = None;
        self.drain()
    }
}

/// Monta quadros H.264 a partir dos pacotes em ordem.
struct FrameAssembler {
    depack: H264Packet,
    frame: BytesMut,
    /// Faltou algo: descarta até o próximo keyframe.
    broken: bool,
    /// Quadro atual perdeu pedaço.
    partial: bool,
    last_ts: Option<u32>,
}

impl FrameAssembler {
    fn new() -> Self {
        Self { depack: H264Packet::default(), frame: BytesMut::new(), broken: true, partial: false, last_ts: None }
    }

    fn mark_broken(&mut self) {
        self.broken = true;
        self.partial = true;
    }

    fn needs_keyframe(&self) -> bool {
        self.broken
    }

    fn push(&mut self, p: &rtc::rtp::Packet, sinks: &ViewerSinks) {
        // Mudou o timestamp sem marcador: o fim do quadro anterior se perdeu.
        if self.last_ts.is_some_and(|ts| ts != p.header.timestamp) && !self.frame.is_empty() {
            self.frame.clear();
            self.depack = H264Packet::default();
            self.partial = false;
        }
        self.last_ts = Some(p.header.timestamp);
        match self.depack.depacketize(&p.payload) {
            Ok(nals) => self.frame.extend_from_slice(&nals),
            Err(_) => self.partial = true,
        }
        if p.header.marker {
            let data = self.frame.split().to_vec();
            let partial = std::mem::take(&mut self.partial);
            self.depack = H264Packet::default();
            if partial {
                self.broken = true;
                return;
            }
            if self.broken && crate::media::has_nal(&data, 5) {
                self.broken = false;
            }
            if !self.broken && !data.is_empty() {
                (sinks.on_video)(data, p.header.timestamp);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn net_meter_measures_rate_loss_and_queue() {
        let t0 = Instant::now();
        let mut m = NetMeter::new(t0);
        let (mut received, mut highest) = (0u64, 1000u64);
        // 30 fps, 4 pacotes de 1000 bytes por quadro, sem fila.
        let frame = |m: &mut NetMeter, i: u32, delay_ms: u64, lose: bool, received: &mut u64, highest: &mut u64| {
            let at = t0 + Duration::from_micros(i as u64 * 33_333 + delay_ms * 1000);
            for k in 0..4 {
                *highest += 1;
                if lose && k == 3 { continue; }
                *received += 1;
                m.on_packet(at, 1000, 7777 + i * 3000);
            }
        };
        assert!(m.report(t0, received, Some(highest)).is_none(), "primeira chamada só marca o início");
        for i in 0..15 { frame(&mut m, i, 0, false, &mut received, &mut highest); }
        let r = m.report(t0 + Duration::from_millis(500), received, Some(highest)).unwrap();
        assert!(r.loss < 0.01 && r.qd < 2.0, "{r:?}");
        assert!((900..1000).contains(&r.kbps), "15 quadros x 4 KB em 0,5 s = 960 kb/s: {r:?}");
        // Fila crescendo (cada quadro 10 ms mais atrasado) e 1 de 4 pacotes perdidos.
        for i in 15..30 { frame(&mut m, i, (i as u64 - 14) * 10, true, &mut received, &mut highest); }
        let r = m.report(t0 + Duration::from_millis(1000), received, Some(highest)).unwrap();
        assert!((r.loss - 0.25).abs() < 0.02, "{r:?}");
        assert!(r.qd > 50.0, "{r:?}");
    }

    /// Etapa 0 da malha (docs/malha-p2p.md): o canal de dados do webrtc-rs aguenta o vídeo?
    /// Canal fora de ordem e sem reenvio, mensagens de 1.100 bytes (um pedaço por pacote),
    /// quadros a 60 fps soltos por um pacer. Mede taxa recebida, perda e atraso de cada
    /// pedaço (mesma máquina: o atraso é só o da pilha SCTP/DTLS/ICE).
    /// `cargo test datachannel_pieces -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn datachannel_pieces_throughput() {
        use webrtc::data_channel::RTCDataChannelInit;
        let t0 = Instant::now();
        let got: Arc<Mutex<Vec<(u64, f64)>>> = Arc::new(Mutex::new(Vec::new()));
        let (gtx_v, mut grx_v) = channel::<()>(1);
        let (gtx_h, mut grx_h) = channel::<()>(1);
        let host_dc: Arc<Mutex<Option<Arc<dyn DataChannel>>>> = Arc::new(Mutex::new(None));
        let hd = host_dc.clone();
        let on_dc: Arc<dyn Fn(Arc<dyn DataChannel>) + Send + Sync> = Arc::new(move |dc| {
            *hd.lock().unwrap() = Some(dc);
        });
        let (ctx, crx) = std::sync::mpsc::channel();
        let hh = Arc::new(Handler {
            gathered: gtx_h,
            on_state: Arc::new(move |e| { if e == PeerEvent::Connected { let _ = ctx.send(()); } }),
            on_track: None,
            on_dc: Some(on_dc),
        });
        let vh = Arc::new(Handler { gathered: gtx_v, on_state: Arc::new(|_| {}), on_track: None, on_dc: None });
        let (host, viewer) = block_on(async { (build_pc(hh, false).await.unwrap(), build_pc(vh, false).await.unwrap()) });
        let init = RTCDataChannelInit { ordered: false, max_retransmits: Some(0), ..Default::default() };
        let vdc = block_on(viewer.create_data_channel("aj-p", Some(init))).unwrap();
        let g = got.clone();
        spawn(async move {
            while let Some(ev) = vdc.poll().await {
                if let DataChannelEvent::OnMessage(m) = ev {
                    let d = &m.data;
                    let seq = u64::from_le_bytes(d[0..8].try_into().unwrap());
                    let sent = u64::from_le_bytes(d[8..16].try_into().unwrap());
                    let now = t0.elapsed().as_micros() as u64;
                    g.lock().unwrap().push((seq, now.saturating_sub(sent) as f64 / 1000.0));
                }
            }
        });
        block_on(async {
            let offer = viewer.create_offer(None).await.unwrap();
            viewer.set_local_description(offer).await.unwrap();
            wait_gathering(&mut grx_v).await;
            let o = viewer.local_description().await.unwrap().sdp;
            host.set_remote_description(RTCSessionDescription::offer(o).unwrap()).await.unwrap();
            let ans = host.create_answer(None).await.unwrap();
            host.set_local_description(ans).await.unwrap();
            wait_gathering(&mut grx_h).await;
            let a = host.local_description().await.unwrap().sdp;
            viewer.set_remote_description(RTCSessionDescription::answer(a).unwrap()).await.unwrap();
        });
        crx.recv_timeout(Duration::from_secs(10)).expect("não conectou");
        let mut dc = None;
        for _ in 0..100 {
            dc = host_dc.lock().unwrap().clone();
            if dc.is_some() { break; }
            std::thread::sleep(Duration::from_millis(50));
        }
        let dc = dc.expect("canal de dados não chegou no host");
        std::thread::sleep(Duration::from_millis(300));

        const PIECE: usize = 1100;
        let mut seq = 0u64;
        for mbps in [8u64, 20, 40] {
            got.lock().unwrap().clear();
            let first = seq;
            let per_frame = ((mbps * 1_000_000 / 8 / 60) as usize).div_ceil(PIECE);
            let pace = Duration::from_secs_f64((PIECE * 8) as f64 / (mbps as f64 * 2.5 * 1e6));
            let secs = 4u32;
            let start = Instant::now();
            for f in 0..(60 * secs) {
                let frame_at = start + Duration::from_micros(f as u64 * 16_667);
                if let Some(w) = frame_at.checked_duration_since(Instant::now()) { std::thread::sleep(w); }
                let dc = dc.clone();
                let n = per_frame;
                let s0 = seq;
                seq += n as u64;
                block_on(async move {
                    let mut next = tokio::time::Instant::now();
                    for i in 0..n as u64 {
                        let mut b = BytesMut::zeroed(PIECE);
                        b[0..8].copy_from_slice(&(s0 + i).to_le_bytes());
                        b[8..16].copy_from_slice(&(t0.elapsed().as_micros() as u64).to_le_bytes());
                        let _ = dc.send(b).await;
                        next += pace;
                        tokio::time::sleep_until(next).await;
                    }
                });
            }
            let sent = seq - first;
            std::thread::sleep(Duration::from_millis(500));
            let g = got.lock().unwrap();
            let mut lat: Vec<f64> = g.iter().map(|x| x.1).collect();
            lat.sort_by(|a, b| a.total_cmp(b));
            let p = |q: f64| lat.get(((lat.len() as f64 * q) as usize).min(lat.len().saturating_sub(1))).copied().unwrap_or(f64::NAN);
            let loss = 1.0 - g.len() as f64 / sent as f64;
            let rate = g.len() as f64 * PIECE as f64 * 8.0 / secs as f64 / 1e6;
            println!(
                "{mbps:>2} Mb/s: recebido {rate:.1} Mb/s, perda {:.2}%, atraso p50 {:.2} ms, p99 {:.2} ms, máx {:.2} ms ({} de {sent} pedaços)",
                loss * 100.0, p(0.5), p(0.99), p(1.0), g.len()
            );
        }
        let _ = block_on(host.close());
        let _ = block_on(viewer.close());
    }

    /// Etapa 1 da malha: vídeo codificado de verdade atravessa o WebRTC em pedaços (canal
    /// "aj-p") e é decodificado do outro lado; depois, com 5% dos pedaços perdidos.
    /// `cargo test loopback_pieces -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn loopback_pieces() {
        crate::media::mf_init().unwrap();
        USE_PIECES.store(true, Ordering::SeqCst);
        for loss in [0u32, 50, 150] {
            TEST_PIECE_LOSS_PERMILLE.store(loss, Ordering::SeqCst);
            KEYFRAME_REQUESTS.store(0, Ordering::SeqCst);
            let decoded = Arc::new(AtomicUsize::new(0));
            let corrupt = Arc::new(AtomicUsize::new(0));
            let (vtx, vrx) = std::sync::mpsc::channel::<(Vec<u8>, u32)>();
            let sinks = ViewerSinks {
                on_video: Arc::new(move |f, ts| { let _ = vtx.send((f, ts)); }),
                on_audio: Arc::new(|_| {}),
                on_state: Arc::new(|_| {}),
                on_message: Arc::new(|_| {}),
                timed: None,
            };
            let (viewer, offer) = block_on(ViewerSession::offer(sinks, false)).expect("oferta");
            let want_kf = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let reports = Arc::new(Mutex::new(Vec::<crate::congestion::Report>::new()));
            let (wk, rr) = (want_kf.clone(), reports.clone());
            // Fonte dos pedaços compartilhada: o espectador pode pedir reparo.
            let source = Arc::new(Mutex::new(crate::pieces::PieceSource::new(PIECE_OVERHEAD)));
            let more_req = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
            let mr = more_req.clone();
            REPAIRS_SENT.store(0, Ordering::SeqCst);
            let (connected_tx, connected_rx) = std::sync::mpsc::channel();
            let (peer, answer) = block_on(HostPeer::answer(
                offer,
                false,
                Arc::new(move |e| { if e == PeerEvent::Connected { let _ = connected_tx.send(()); } }),
                Arc::new(move |m| {
                    if m.contains("\"kf\"") { wk.store(true, Ordering::SeqCst); }
                    if let Ok(r) = serde_json::from_str::<crate::congestion::Report>(&m) { rr.lock().unwrap().push(r); }
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&m) {
                        if v["t"] == "more" { mr.lock().unwrap().push(v); }
                    }
                }),
            ))
            .expect("resposta");
            block_on(viewer.accept_answer(answer)).expect("aceitar resposta");
            connected_rx.recv_timeout(Duration::from_secs(10)).expect("não conectou");
            // Espera o canal dos pedaços chegar no host (antes disso iria por RTP).
            for _ in 0..100 {
                if peer.pieces_dc.lock().unwrap().is_some() { break; }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(peer.pieces_dc.lock().unwrap().is_some(), "canal de pedaços não abriu");

            let (d, c) = (decoded.clone(), corrupt.clone());
            let dec_thread = std::thread::spawn(move || {
                crate::media::mf_init().unwrap();
                let mut dec = crate::media::Decoder::new().unwrap();
                let mut last_ts = None::<u32>;
                while let Ok((f, ts)) = vrx.recv_timeout(Duration::from_secs(2)) {
                    // Na ordem: cada timestamp maior que o anterior.
                    if last_ts.is_some_and(|l| ts.wrapping_sub(l) as i32 <= 0) { c.fetch_add(1, Ordering::SeqCst); }
                    last_ts = Some(ts);
                    match dec.decode(&f, 0) {
                        Ok(frames) => for fr in frames { assert_eq!((fr.width, fr.height), (1280, 720)); d.fetch_add(1, Ordering::SeqCst); },
                        Err(_) => { c.fetch_add(1, Ordering::SeqCst); }
                    }
                }
            });

            let conv = crate::media::Converter::new(1280, 720, 1280, 720, 60).unwrap();
            let mut enc = crate::media::Encoder::new(1280, 720, 60, 4_000_000, true, None).unwrap();

            let mut img = vec![0u8; 1280 * 720 * 4];
            let frames = 240u32;
            for i in 0..frames {
                if want_kf.swap(false, Ordering::SeqCst) { enc.force_keyframe(); }
                for (j, px) in img.chunks_exact_mut(4).enumerate() {
                    px[0] = ((j as u32 + i * 7) % 256) as u8; px[1] = (i % 256) as u8; px[2] = 90; px[3] = 255;
                }
                let nv12 = conv.convert(&img, i as i64 * 166_667, 166_667).unwrap();
                for pkt in enc.encode(&nv12).unwrap() {
                    let f = source.lock().unwrap().encode(0, &pkt.data, Duration::from_micros(16_667));
                    if let Some(f) = f {
                        peer.send_pieces(&f);
                    }
                }
                // Pedidos de reparo (como a sessão faz).
                for v in more_req.lock().unwrap().drain(..) {
                    let blocks: Vec<(u8, u32)> = v["b"].as_array().map(|a| a.iter().filter_map(|x| Some((x[0].as_u64()? as u8, x[1].as_u64()? as u32))).collect()).unwrap_or_default();
                    let extra = source.lock().unwrap().more(v["r"].as_u64().unwrap_or(0) as u32, v["f"].as_u64().unwrap_or(0) as u32, &blocks);
                    peer.send_extra(extra);
                }
                std::thread::sleep(Duration::from_millis(16));
            }
            std::thread::sleep(Duration::from_millis(500));
            let _ = block_on(peer.pc.close());
            let _ = block_on(viewer.pc.close());
            dec_thread.join().unwrap();
            let reports = reports.lock().unwrap();
            let mean_loss = reports.iter().map(|r| r.loss).sum::<f32>() / reports.len().max(1) as f32;
            println!(
                "perda simulada {:.1}%: decodificados {} de {frames}, fora de ordem/erros {}, pedidos de keyframe {}, reparos {}, relatórios {} (perda média medida {:.1}%)",
                loss as f32 / 10.0, decoded.load(Ordering::SeqCst), corrupt.load(Ordering::SeqCst),
                KEYFRAME_REQUESTS.load(Ordering::SeqCst), REPAIRS_SENT.load(Ordering::SeqCst), reports.len(), mean_loss * 100.0
            );
            assert_eq!(corrupt.load(Ordering::SeqCst), 0, "quadro fora de ordem ou que não decodificou");
            assert!(reports.len() >= 4, "o espectador devia mandar relatórios");
            if loss == 0 {
                assert!(decoded.load(Ordering::SeqCst) >= frames as usize - 10);
                assert!(mean_loss < 0.01);
            } else if loss == 50 {
                // 5% de perda com 25% de folga: quase todos os quadros fecham.
                assert!(decoded.load(Ordering::SeqCst) >= frames as usize * 85 / 100);
                assert!((mean_loss - 0.05).abs() < 0.03, "a perda medida devia ser ~5%: {mean_loss}");
            } else {
                // 15% de perda passa da folga em parte dos quadros: o reparo salva.
                assert!(REPAIRS_SENT.load(Ordering::SeqCst) > 0, "devia ter pedido reparo");
                // (varia entre rodadas: 198 a 240 de 240 medidos)
                assert!(decoded.load(Ordering::SeqCst) >= frames as usize * 80 / 100);
            }
        }
        TEST_PIECE_LOSS_PERMILLE.store(0, Ordering::SeqCst);
        USE_PIECES.store(false, Ordering::SeqCst);
    }

    /// Host e espectador na mesma máquina: vídeo codificado de verdade atravessa o WebRTC
    /// e é decodificado do outro lado.
    #[test]
    #[ignore]
    fn loopback_video() {
        crate::media::mf_init().unwrap();
        let received = Arc::new(AtomicUsize::new(0));
        let decoded = Arc::new(AtomicUsize::new(0));
        let keyframe_requests = Arc::new(AtomicUsize::new(0));
        let (vtx, vrx) = std::sync::mpsc::channel::<Vec<u8>>();

        let r = received.clone();
        let sinks = ViewerSinks {
            on_video: Arc::new(move |f, _| { r.fetch_add(1, Ordering::SeqCst); let _ = vtx.send(f); }),
            on_audio: Arc::new(|_| {}),
            on_state: Arc::new(|e| println!("espectador: {e:?}")),
            on_message: Arc::new(|m| println!("espectador recebeu: {m}")),
            timed: None,
        };
        let (viewer, offer) = block_on(ViewerSession::offer(sinks, false)).expect("oferta");
        let kf = keyframe_requests.clone();
        let reports = Arc::new(Mutex::new(Vec::<crate::congestion::Report>::new()));
        let rr = reports.clone();
        let (connected_tx, connected_rx) = std::sync::mpsc::channel();
        let (peer, answer) = block_on(HostPeer::answer(
            offer,
            false,
            Arc::new(move |e| { println!("host: {e:?}"); if e == PeerEvent::Connected { let _ = connected_tx.send(()); } }),
            Arc::new(move |m| {
                if m.contains("\"kf\"") { kf.fetch_add(1, Ordering::SeqCst); }
                if let Ok(r) = serde_json::from_str::<crate::congestion::Report>(&m) { rr.lock().unwrap().push(r); }
            }),
        ))
        .expect("resposta");
        block_on(viewer.accept_answer(answer)).expect("aceitar resposta");
        connected_rx.recv_timeout(Duration::from_secs(10)).expect("não conectou");

        // Decoder do espectador numa thread.
        let d = decoded.clone();
        let dec_thread = std::thread::spawn(move || {
            crate::media::mf_init().unwrap();
            let mut dec = crate::media::Decoder::new().unwrap();
            while let Ok(f) = vrx.recv_timeout(Duration::from_secs(2)) {
                if let Ok(frames) = dec.decode(&f, 0) {
                    for fr in frames { assert_eq!((fr.width, fr.height), (1280, 720)); d.fetch_add(1, Ordering::SeqCst); }
                }
            }
        });

        let conv = crate::media::Converter::new(1280, 720, 1280, 720, 60).unwrap();
        let mut enc = crate::media::Encoder::new(1280, 720, 60, 4_000_000, true, None).unwrap();
        let mut img = vec![0u8; 1280 * 720 * 4];
        let start = Instant::now();
        let (mut t_conv, mut t_enc, mut t_send) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
        for i in 0..180u32 {
            for (j, px) in img.chunks_exact_mut(4).enumerate() {
                px[0] = ((j as u32 + i * 7) % 256) as u8; px[1] = (i % 256) as u8; px[2] = 90; px[3] = 255;
            }
            let t = i as i64 * 166_667;
            let a = Instant::now();
            let nv12 = conv.convert(&img, t, 166_667).unwrap();
            let b = Instant::now();
            let pkts = enc.encode(&nv12).unwrap();
            let c2 = Instant::now();
            for pkt in pkts {
                block_on(peer.send_video(Bytes::from(pkt.data), Duration::from_micros(16_667))).unwrap();
            }
            t_conv += b - a; t_enc += c2 - b; t_send += c2.elapsed();
            std::thread::sleep(Duration::from_millis(16));
        }
        std::thread::sleep(Duration::from_millis(500));
        let _ = block_on(peer.pc.close());
        dec_thread.join().unwrap();
        println!("tempo por quadro: conversão {:?}, encode {:?}, envio {:?}", t_conv / 180, t_enc / 180, t_send / 180);
        println!(
            "enviados 180 em {:?}; recebidos {}, decodificados {}, pedidos de keyframe {}",
            start.elapsed(), received.load(Ordering::SeqCst), decoded.load(Ordering::SeqCst), keyframe_requests.load(Ordering::SeqCst)
        );
        assert!(decoded.load(Ordering::SeqCst) >= 150);
        let reports = reports.lock().unwrap();
        println!("relatórios do espectador: {}; últimos: {:?}", reports.len(), &reports[reports.len().saturating_sub(3)..]);
        assert!(reports.len() >= 4, "o espectador devia mandar um relatório a cada 500 ms");
        assert!(reports.iter().all(|r| r.loss < 0.05), "perda na mesma máquina? {reports:?}");
    }

    fn stats(label: &str, times: &[Instant]) {
        let iv: Vec<f64> = times.windows(2).map(|w| (w[1] - w[0]).as_secs_f64() * 1000.0).collect();
        if iv.is_empty() { println!("{label}: sem quadros"); return; }
        let mean = iv.iter().sum::<f64>() / iv.len() as f64;
        let sd = (iv.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / iv.len() as f64).sqrt();
        let mut sorted = iv.clone(); sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p99 = sorted[(sorted.len() as f64 * 0.99) as usize - 1];
        let max = sorted.last().unwrap();
        let over = iv.iter().filter(|&&x| x > 25.0).count();
        let secs = (times.last().unwrap().duration_since(times[0])).as_secs_f64();
        println!("{label}: {:.1} fps | intervalo médio {:.2} ms, desvio {:.2} ms, p99 {:.1} ms, máx {:.1} ms, {} engasgos (>25 ms)",
            times.len() as f64 / secs, mean, sd, p99, max, over);
    }

    /// Troca de versão no meio: o espectador recebe a 1080p e passa para a 720p exatamente
    /// num keyframe dela (como a sessão do host faz). O decoder tem que trocar de tamanho
    /// sem nenhum erro e sem quadro corrompido.
    /// `cargo test rendition_switch -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn rendition_switch_over_webrtc() {
        use crate::video::{Callbacks, ExtraReq, VideoPipeline, VideoSettings};
        use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
        crate::media::mf_init().unwrap();
        BROKEN_FRAMES.store(0, SeqCst);
        let (vtx, vrx) = std::sync::mpsc::channel::<Vec<u8>>();
        let sinks = ViewerSinks {
            on_video: Arc::new(move |f, _| { let _ = vtx.send(f); }),
            on_audio: Arc::new(|_| {}),
            on_state: Arc::new(|_| {}),
            on_message: Arc::new(|_| {}),
            timed: None,
        };
        let (viewer, offer) = block_on(ViewerSession::offer(sinks, false)).unwrap();
        let (ctx, crx) = std::sync::mpsc::channel();
        let (peer, answer) = block_on(HostPeer::answer(offer, false,
            Arc::new(move |e| if e == PeerEvent::Connected { let _ = ctx.send(()); }),
            Arc::new(|_| {}))).unwrap();
        block_on(viewer.accept_answer(answer)).unwrap();
        crx.recv_timeout(Duration::from_secs(10)).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        // Versão atual do espectador e a pendente (0 = nenhuma), como no PeerEntry.
        let current = Arc::new(AtomicU32::new(0));
        let pending = Arc::new(AtomicU32::new(u32::MAX));
        let (cur, pen, p2) = (current.clone(), pending.clone(), peer.clone());
        let src = crate::capture::list_sources().into_iter().find(|s| s.primary).unwrap();
        let pipe = VideoPipeline::start(src.id, VideoSettings { res: 1080, fps: 30, bitrate: 8000, cursor: true }, Callbacks {
            on_packet: Arc::new(move |key, d, dur, _| {
                if pen.load(SeqCst) == key && crate::media::has_nal(&d, 5) {
                    cur.store(key, SeqCst);
                    pen.store(u32::MAX, SeqCst);
                }
                if cur.load(SeqCst) == key {
                    let p = p2.clone();
                    let _ = block_on(p.send_video(Bytes::from(d), dur));
                }
            }),
            on_info: Arc::new(|_| {}), on_preview: Arc::new(|_| {}),
            on_error: Arc::new(|e| println!("ERRO {e}")), on_source_closed: Arc::new(|| {}),
        }).unwrap();
        pipe.keyframe();
        std::thread::sleep(Duration::from_secs(2));
        // Pede a 720p e troca no keyframe dela.
        pipe.set_extras(vec![ExtraReq { height: 720, cap_bps: None }]);
        pending.store(720, SeqCst);
        pipe.keyframe_for(720);
        std::thread::sleep(Duration::from_secs(2));
        // E volta para a principal.
        pending.store(0, SeqCst);
        pipe.keyframe_for(0);
        std::thread::sleep(Duration::from_secs(2));
        pipe.stop();
        std::thread::sleep(Duration::from_millis(300));
        let _ = block_on(peer.pc.close());
        let _ = block_on(viewer.pc.close());

        let mut dec = crate::media::Decoder::new().unwrap();
        let (mut sizes, mut errors, mut frames) = (Vec::<(u32, u32)>::new(), 0, 0);
        while let Ok(f) = vrx.recv_timeout(Duration::from_millis(500)) {
            match dec.decode(&f, 0) {
                Ok(out) => {
                    for fr in out {
                        frames += 1;
                        if sizes.last() != Some(&(fr.width, fr.height)) {
                            sizes.push((fr.width, fr.height));
                        }
                    }
                }
                Err(_) => errors += 1,
            }
        }
        println!("{frames} quadros, tamanhos em ordem: {sizes:?}, erros no decoder: {errors}, quadros corrompidos na rede: {}", BROKEN_FRAMES.load(SeqCst));
        assert_eq!(sizes.iter().map(|s| s.1).collect::<Vec<_>>(), vec![1080, 720, 1080], "devia ir 1080 -> 720 -> 1080");
        assert_eq!(errors, 0);
        assert_eq!(BROKEN_FRAMES.load(SeqCst), 0);
        assert!(frames > 150);
    }

    /// Captura real → encoder → WebRTC → receptor → decoder, medindo a regularidade.
    #[test]
    #[ignore]
    fn e2e_capture_stability() {
        use crate::video::{Callbacks, VideoPipeline, VideoSettings};
        use std::sync::atomic::Ordering::SeqCst;
        crate::media::mf_init().unwrap();
        for (reorder, smooth) in [(0u32, true), (20, true), (50, true)] {
            TEST_REORDER_PERMILLE.store(reorder, SeqCst);
            BROKEN_FRAMES.store(0, SeqCst);
            KEYFRAME_REQUESTS.store(0, SeqCst);
            let (vtx, vrx) = std::sync::mpsc::channel::<(Vec<u8>, u32)>();
            let (frames_in, queued, dropped) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let fi = frames_in.clone();
            let sinks = ViewerSinks {
                on_video: Arc::new(move |f, ts| { fi.fetch_add(1, SeqCst); let _ = vtx.send((f, ts)); }),
                on_audio: Arc::new(|_| {}),
                on_state: Arc::new(|_| {}),
                on_message: Arc::new(|_| {}),
                timed: None,
            };
            let (viewer, offer) = block_on(ViewerSession::offer(sinks, false)).unwrap();
            let host_times = Arc::new(Mutex::new(Vec::<Instant>::new()));
            let (ctx, crx) = std::sync::mpsc::channel();
            let kf_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let kf2 = kf_flag.clone();
            let (peer, answer) = block_on(HostPeer::answer(offer, false,
                Arc::new(move |e| if e == PeerEvent::Connected { let _ = ctx.send(()); }),
                Arc::new(move |m| if m.contains("kf") { kf2.store(true, SeqCst); }))).unwrap();
            block_on(viewer.accept_answer(answer)).unwrap();
            crx.recv_timeout(Duration::from_secs(10)).unwrap();

            let dec_thread = std::thread::spawn(move || {
                crate::media::mf_init().unwrap();
                let mut dec = crate::media::Decoder::new().unwrap();
                let mut playout = crate::playout::Playout::new();
                let mut times = Vec::new();
                let (mut ok, mut errs) = (0usize, 0usize);
                // O 1º quadro decodificável (keyframe) pode levar alguns segundos para chegar.
                let mut wait = Duration::from_secs(10);
                while let Ok((f, ts)) = vrx.recv_timeout(wait) {
                    wait = Duration::from_secs(2);
                    let res = dec.decode(&f, 0);
                    match &res {
                        Ok(o) => ok += o.len(),
                        Err(e) => { if errs == 0 { println!("  erro no decoder: {e}"); } errs += 1; }
                    }
                    if let Ok(out) = res {
                        for _ in out {
                            if smooth {
                                crate::playout::sleep_until(playout.schedule(ts));
                            }
                            times.push(Instant::now());
                        }
                    }
                }
                println!("  decoder: {ok} quadros, {errs} erros");
                times
            });

            let src = crate::capture::list_sources().into_iter().find(|s| s.primary).unwrap();
            let (p2, ht, q2, d2) = (peer.clone(), host_times.clone(), queued.clone(), dropped.clone());
            let pipe = VideoPipeline::start(src.id, VideoSettings { res: 1080, fps: 60, bitrate: 10000, cursor: true }, Callbacks {
                on_packet: Arc::new(move |_, d, dur, _| {
                    ht.lock().unwrap().push(Instant::now());
                    let p = p2.clone();
                    let (q, dr) = (q2.clone(), d2.clone());
                    spawn(async move {
                        match p.send_video(Bytes::from(d), dur).await {
                            Ok(true) => q.fetch_add(1, SeqCst),
                            _ => dr.fetch_add(1, SeqCst),
                        };
                    });
                }),
                on_info: Arc::new(|_| {}), on_preview: Arc::new(|_| {}),
                on_error: Arc::new(|e| println!("ERRO {e}")), on_source_closed: Arc::new(|| {}),
            }).unwrap();
            for _ in 0..12 {
                std::thread::sleep(Duration::from_millis(500));
                if kf_flag.swap(false, SeqCst) { pipe.keyframe(); }
            }
            pipe.stop();
            std::thread::sleep(Duration::from_millis(300));
            let _ = block_on(peer.pc.close());
            let _ = block_on(viewer.pc.close());
            let times = dec_thread.join().unwrap();
            println!("--- {} pacotes fora de ordem a cada 1000, buffer de reprodução: {} ---", reorder, if smooth { "ligado" } else { "desligado" });
            let ht = host_times.lock().unwrap();
            stats("  host (saída do encoder)", &ht[ht.len().min(60)..]);
            stats("  espectador (decodificados)", &times[times.len().min(60)..]);
            println!("  quadros descartados: {}, pedidos de keyframe: {}", BROKEN_FRAMES.load(SeqCst), KEYFRAME_REQUESTS.load(SeqCst));
            println!("  host: {} quadros no pacer, {} recusados; espectador: {} quadros completos", queued.load(SeqCst), dropped.load(SeqCst), frames_in.load(SeqCst));
        }
        TEST_REORDER_PERMILLE.store(0, SeqCst);
    }
}
