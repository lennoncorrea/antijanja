//! Transporte WebRTC nativo (webrtc-rs): conexão P2P com NAT traversal, DTLS-SRTP,
//! H.264 + Opus. A troca de oferta/resposta continua pela DHT (módulo `rendezvous`).
//!
//! - Host: uma `PeerConnection` por espectador, com uma faixa de vídeo e uma de áudio.
//!   O encoder roda uma vez só e os pacotes são distribuídos para todos.
//! - Espectador: recebe RTP, remonta os quadros H.264 e entrega Opus cru.
//! - Canal de dados "aj": pedidos de keyframe e aviso de saída.

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
fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

const H264_PT: u8 = 102;

/// Contadores de diagnóstico do receptor (quadros descartados e pedidos de keyframe).
pub static BROKEN_FRAMES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub static KEYFRAME_REQUESTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Só para testes: embaralha a ordem de chegada (por mil pacotes), simulando rede real.
pub static TEST_REORDER_PERMILLE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const OPUS_PT: u8 = 111;
const ICE_TIMEOUT: Duration = Duration::from_secs(4);
const RTP_MTU: usize = 1200;
/// Ritmo do pacer em relação ao bitrate alvo: folga para keyframes (o libwebrtc usa 2,5x).
const PACING_FACTOR: f64 = 2.5;
const MIN_PACING_BPS: u64 = 2_000_000;
/// Rajada máxima que o pacer solta de uma vez depois de ficar parado.
const PACER_BURST: Duration = Duration::from_millis(5);
/// Quadros esperando no pacer (~0,75 s a 60 fps). Cheio: a rede não está dando conta.
const PACER_QUEUE: usize = 45;

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

/// Monta a PeerConnection com nossos codecs e STUN.
async fn build_pc(handler: Arc<dyn PeerConnectionEventHandler>, stun: bool) -> Res<Arc<dyn PeerConnection>> {
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
        .with_udp_addrs(vec!["0.0.0.0:0"])
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

struct Handler {
    gathered: Sender<()>,
    on_state: Arc<dyn Fn(PeerEvent) + Send + Sync>,
    on_track: Option<Arc<dyn Fn(Arc<dyn TrackRemote>) + Send + Sync>>,
    on_dc: Option<Arc<dyn Fn(Arc<dyn DataChannel>) + Send + Sync>>,
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

async fn wait_gathering(rx: &mut webrtc::runtime::Receiver<()>) {
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
        let slot = dc_slot.clone();
        let on_dc: Arc<dyn Fn(Arc<dyn DataChannel>) + Send + Sync> = Arc::new(move |dc: Arc<dyn DataChannel>| {
            *slot.lock().unwrap() = Some(dc.clone());
            let on_message = on_message.clone();
            spawn(async move {
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
        let sdp = pc.local_description().await.ok_or("sem descrição local")?.sdp;

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
        let peer = Arc::new(HostPeer {
            pc, audio, assrc,
            packetizer: Mutex::new(packetizer),
            pacer,
            pacing_bps,
            pts: Mutex::new(None),
            senders: vec![vs, a_s],
            dc: Mutex::new(None),
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

    pub async fn send_message(&self, text: &str) {
        let dc = self.dc.lock().unwrap().clone();
        if let Some(dc) = dc {
            let _ = dc.send_text(text).await;
        }
    }
}

/* ========================================================================== */
/*                                 ESPECTADOR                                  */
/* ========================================================================== */

pub struct ViewerSession {
    pub pc: Arc<dyn PeerConnection>,
    dc: Arc<dyn DataChannel>,
}

/// Callbacks de mídia do espectador.
#[derive(Clone)]
pub struct ViewerSinks {
    /// Quadro H.264 completo (Annex B) + timestamp RTP (90 kHz).
    pub on_video: Arc<dyn Fn(Vec<u8>, u32) + Send + Sync>,
    /// Pacote Opus.
    pub on_audio: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
    pub on_state: Arc<dyn Fn(PeerEvent) + Send + Sync>,
    pub on_message: Arc<dyn Fn(String) + Send + Sync>,
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
        let on_message = sinks.on_message.clone();
        let dcc = dc.clone();
        spawn(async move {
            while let Some(ev) = dcc.poll().await {
                match ev {
                    DataChannelEvent::OnMessage(m) => on_message(String::from_utf8_lossy(&m.data).into_owned()),
                    DataChannelEvent::OnClose => break,
                    _ => {}
                }
            }
        });

        let offer = pc.create_offer(None).await.map_err(err)?;
        pc.set_local_description(offer).await.map_err(err)?;
        wait_gathering(&mut grx).await;
        let sdp = pc.local_description().await.ok_or("sem descrição local")?.sdp;
        Ok((Arc::new(ViewerSession { pc, dc }), sdp))
    }

    pub async fn accept_answer(&self, sdp: String) -> Res<()> {
        self.pc.set_remote_description(RTCSessionDescription::answer(sdp).map_err(err)?).await.map_err(err)
    }

    pub async fn send_message(&self, text: &str) {
        let _ = self.dc.send_text(text).await;
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
                on_packet: Arc::new(move |d, dur| {
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
