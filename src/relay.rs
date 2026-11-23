//! Repasse por outros usuários (docs/repasse.md): quem tem a porta aberta no roteador
//! (UPnP) e IP público ajuda quem não consegue conexão direta, repassando a conexão por um
//! servidor TURN pequeno dentro do próprio app.
//!
//! - Ligado por padrão (opção "Ajudar quem não consegue conectar"), com limite: uma
//!   conexão repassada por vez e até `MAX_BPS` em cada sentido (o que passar disso é
//!   descartado, como numa internet cheia).
//! - O app anuncia aos vizinhos da fofoca que pode repassar. Quem precisa pede uma
//!   credencial (`issue`), que vale para uma conexão só.
//! - O TURN é o padrão do WebRTC: o conteúdo continua cifrado de ponta a ponta (DTLS/SRTP);
//!   quem repassa só vê pacotes passando.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use turn::auth::{generate_auth_key, AuthHandler};
use turn::relay::RelayAddressGenerator;
use turn::server::config::{ConnConfig, ServerConfig};
use turn::server::Server;
use webrtc_util::Conn;

const REALM: &str = "antijanja";
/// Limite de banda de cada sentido da conexão repassada.
pub const MAX_BPS: u64 = 10_000_000;
/// Credencial que não foi usada em tanto tempo deixa de valer.
const UNUSED_TTL: Duration = Duration::from_secs(60);
/// E uma em uso vale até a conexão fechar (ou por no máximo tanto tempo).
const MAX_SESSION: Duration = Duration::from_secs(6 * 60 * 60);

fn log(msg: impl AsRef<str>) {
    crate::connlog::log(format!("repasse: {}", msg.as_ref()));
}

/// Credencial para usar o repasse de alguém.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnCreds {
    pub url: String,
    pub user: String,
    pub pass: String,
}

struct Cred {
    pass: String,
    issued: Instant,
    used: bool,
}

#[derive(Default)]
struct St {
    /// Endereço público do TURN (anunciado aos vizinhos), com o servidor rodando.
    public: Option<SocketAddrV4>,
    server: Option<Arc<Server>>,
    creds: HashMap<String, Cred>,
    /// Conexões repassadas agora.
    active: usize,
}

fn st() -> &'static Mutex<St> {
    static ST: OnceLock<Mutex<St>> = OnceLock::new();
    ST.get_or_init(|| Mutex::new(St::default()))
}

static ENABLED: AtomicBool = AtomicBool::new(true);

/// O app pode repassar agora? Devolve o endereço do TURN.
pub fn advertised() -> Option<String> {
    let s = st().lock().unwrap();
    (ENABLED.load(Ordering::SeqCst) && s.server.is_some() && s.active == 0).then(|| s.public.map(|a| a.to_string())).flatten()
}

/// Liga ou desliga (opção do usuário). Desligar fecha o servidor (e a conexão repassada).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::SeqCst);
    if !on {
        let server = st().lock().unwrap().server.take();
        if let Some(s) = server {
            log("desligado");
            crate::transport::spawn(async move {
                let _ = s.close().await;
            });
        }
    }
}

/// Uma credencial para uma conexão (se houver vaga).
pub fn issue() -> Option<TurnCreds> {
    let mut s = st().lock().unwrap();
    s.creds.retain(|_, c| if c.used { c.issued.elapsed() < MAX_SESSION } else { c.issued.elapsed() < UNUSED_TTL });
    // Uma por vez: enquanto houver uma valendo (em uso, ou entregue há pouco e ainda
    // não usada), ninguém mais recebe.
    if !ENABLED.load(Ordering::SeqCst) || s.server.is_none() || s.active > 0 || !s.creds.is_empty() {
        return None;
    }
    let public = s.public?;
    let (user, pass) = (random_hex(8), random_hex(16));
    s.creds.insert(user.clone(), Cred { pass: pass.clone(), issued: Instant::now(), used: false });
    log("credencial entregue a um vizinho");
    Some(TurnCreds { url: format!("turn:{public}?transport=udp"), user, pass })
}

fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Auth;

impl AuthHandler for Auth {
    fn auth_handle(&self, username: &str, realm: &str, _src: SocketAddr) -> Result<Vec<u8>, turn::Error> {
        let mut s = st().lock().unwrap();
        let ok = ENABLED.load(Ordering::SeqCst);
        match s.creds.get_mut(username) {
            Some(c) if ok && (c.used || c.issued.elapsed() < UNUSED_TTL) => {
                if !c.used {
                    c.used = true;
                    log("conexão repassada começou");
                }
                Ok(generate_auth_key(username, realm, &c.pass))
            }
            _ => Err(turn::Error::ErrFakeErr),
        }
    }
}

/// Gera o endereço repassado: uma porta fixa (aberta no roteador), uma conexão por vez,
/// com limite de banda.
struct RelayGen {
    public_ip: IpAddr,
    port: u16,
}

#[async_trait::async_trait]
impl RelayAddressGenerator for RelayGen {
    fn validate(&self) -> Result<(), turn::Error> {
        Ok(())
    }

    fn supports_family(&self, use_ipv4: bool) -> bool {
        use_ipv4
    }

    async fn allocate_conn(&self, use_ipv4: bool, _requested_port: u16) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), turn::Error> {
        if !use_ipv4 {
            return Err(turn::Error::ErrInvalidRequestedFamilyValue);
        }
        let sock = UdpSocket::bind(("0.0.0.0", self.port)).await.map_err(|e| turn::Error::Other(e.to_string()))?;
        {
            let mut s = st().lock().unwrap();
            s.active += 1;
        }
        let conn: Arc<dyn Conn + Send + Sync> = Arc::new(Throttled { sock, up: Mutex::new(Bucket::new()), down: Mutex::new(Bucket::new()) });
        Ok((conn, SocketAddr::new(self.public_ip, self.port)))
    }
}

/// Balde de fichas: até `MAX_BPS`, com folga de 1/4 s.
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn new() -> Self {
        Bucket { tokens: MAX_BPS as f64 / 32.0, at: Instant::now() }
    }

    /// Cabe mais `bytes` agora?
    fn take(&mut self, bytes: usize) -> bool {
        let cap = MAX_BPS as f64 / 32.0; // 1/4 s, em bytes
        self.tokens = (self.tokens + self.at.elapsed().as_secs_f64() * MAX_BPS as f64 / 8.0).min(cap);
        self.at = Instant::now();
        if self.tokens >= bytes as f64 {
            self.tokens -= bytes as f64;
            true
        } else {
            false
        }
    }
}

struct Throttled {
    sock: UdpSocket,
    up: Mutex<Bucket>,
    down: Mutex<Bucket>,
}

#[async_trait::async_trait]
impl Conn for Throttled {
    async fn connect(&self, addr: SocketAddr) -> webrtc_util::Result<()> {
        Ok(self.sock.connect(addr).await?)
    }
    async fn recv(&self, buf: &mut [u8]) -> webrtc_util::Result<usize> {
        Ok(self.recv_from(buf).await?.0)
    }
    async fn recv_from(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        loop {
            let (n, from) = self.sock.recv_from(buf).await?;
            if self.down.lock().unwrap().take(n) {
                return Ok((n, from));
            }
        }
    }
    async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
        Ok(self.sock.send(buf).await?)
    }
    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        if !self.up.lock().unwrap().take(buf.len()) {
            return Ok(buf.len()); // passou do limite: descarta
        }
        Ok(self.sock.send_to(buf, target).await?)
    }
    fn local_addr(&self) -> webrtc_util::Result<SocketAddr> {
        Ok(self.sock.local_addr()?)
    }
    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }
    async fn close(&self) -> webrtc_util::Result<()> {
        Ok(())
    }
    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

/// Liga o servidor de repasse quando a porta do roteador estiver aberta (em segundo
/// plano; tenta de novo se a rede mudar). Precisa de UPnP e de IP público.
pub fn start(enabled: bool) {
    ENABLED.store(enabled, Ordering::SeqCst);
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) || cfg!(test) {
        return;
    }
    std::thread::Builder::new()
        .name("aj-repasse".into())
        .spawn(|| {
            // Portas abertas no roteador e quando foram renovadas.
            let mut ports: Option<([u16; 2], Instant)> = None;
            loop {
                let running = st().lock().unwrap().server.is_some();
                if ENABLED.load(Ordering::SeqCst) && !running {
                    if let Some((listen, relay)) = crate::upnp::open_relay_ports() {
                        match crate::transport::block_on(run(listen, relay)) {
                            Ok(()) => {
                                log(format!("pronto para ajudar quem não consegue conectar (porta {})", listen.port()));
                                crate::gossip::announce_relay();
                                ports = Some(([listen.port(), relay.port()], Instant::now()));
                            }
                            Err(e) => log(format!("não iniciou: {e}")),
                        }
                    }
                } else if let Some((p, at)) = ports.filter(|_| running) {
                    // Renova antes de a abertura (1 h) vencer.
                    if at.elapsed() >= Duration::from_secs(30 * 60) {
                        if crate::upnp::renew(&p) {
                            ports = Some((p, Instant::now()));
                        } else {
                            log("o roteador não renovou as portas: repasse fechado até abrir de novo");
                            let was = ENABLED.load(Ordering::SeqCst);
                            set_enabled(false);
                            ENABLED.store(was, Ordering::SeqCst);
                            ports = None;
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(60));
            }
        })
        .ok();
}

async fn run(listen: SocketAddrV4, relay: SocketAddrV4) -> Result<(), String> {
    let conn = Arc::new(UdpSocket::bind(("0.0.0.0", listen.port())).await.map_err(|e| e.to_string())?);
    let (close_tx, mut close_rx) = tokio::sync::mpsc::channel(4);
    let server = Server::new(ServerConfig {
        conn_configs: vec![ConnConfig { conn, relay_addr_generator: Box::new(RelayGen { public_ip: IpAddr::V4(*relay.ip()), port: relay.port() }) }],
        realm: REALM.into(),
        auth_handler: Arc::new(Auth),
        channel_bind_timeout: Duration::from_secs(0),
        alloc_close_notify: Some(close_tx),
    })
    .await
    .map_err(|e| e.to_string())?;
    {
        let mut s = st().lock().unwrap();
        s.server = Some(Arc::new(server));
        s.public = Some(listen);
    }
    tokio::spawn(async move {
        while let Some(info) = close_rx.recv().await {
            let mut s = st().lock().unwrap();
            s.active = s.active.saturating_sub(1);
            s.creds.remove(&info.username);
            drop(s);
            log("conexão repassada terminou");
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Conexão WebRTC de verdade (canal de dados da malha) que só pode passar pelo repasse:
    /// um lado só tem o candidato do TURN deste app. Os pedaços chegam do outro lado; a
    /// credencial vale para uma conexão só.
    #[test]
    fn webrtc_connects_through_the_relay() {
        use crate::meshnet::MeshLink;
        use crate::transport::block_on;
        let free = || std::net::UdpSocket::bind("0.0.0.0:0").unwrap().local_addr().unwrap().port();
        let (lp, rp) = (free(), free());
        // As conexões não usam o loopback: o TURN é anunciado no IP da rede local.
        let Some(IpAddr::V4(lan)) = crate::gossip::lan_ip() else { panic!("sem rede local") };
        block_on(run(SocketAddrV4::new(lan, lp), SocketAddrV4::new(lan, rp))).unwrap();
        let creds = issue().expect("credencial");
        assert!(issue().is_none(), "uma credencial por vez");
        *crate::transport::TEST_RELAY_ONLY.lock().unwrap() = Some(creds);

        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let tx = Mutex::new(tx);
        let (a, offer) = block_on(MeshLink::offer(false)).unwrap();
        assert!(offer.contains("typ relay"), "a oferta devia ter só o candidato do repasse:
{offer}");
        assert!(!offer.contains("typ host"), "a oferta não devia ter candidato direto");
        let (_b, answer) = block_on(MeshLink::answer(offer, false, Arc::new(move |p| {
            let _ = tx.lock().unwrap().send(p);
        })))
        .unwrap();
        block_on(a.accept_answer(answer)).unwrap();
        let until = Instant::now() + Duration::from_secs(15);
        let mut got = None;
        while got.is_none() && Instant::now() < until {
            block_on(a.send(b"pelo repasse"));
            got = rx.recv_timeout(Duration::from_millis(200)).ok();
        }
        // (a malha põe o número do pedaço, 4 bytes, na frente)
        assert!(got.as_deref().is_some_and(|g| g.ends_with(b"pelo repasse")), "nada chegou pelo repasse");
        assert_eq!(st().lock().unwrap().active, 1, "uma conexão repassada");
        assert!(advertised().is_none(), "ocupado: não se anuncia");
        a.close();
    }

    #[test]
    fn bucket_limits_the_rate() {
        let mut b = Bucket::new();
        // Começa com 1/4 s de folga (312 KB a 10 Mb/s) e não passa disso de uma vez.
        let mut sent = 0usize;
        while b.take(1200) {
            sent += 1200;
        }
        assert!(sent <= (MAX_BPS / 32) as usize && sent > (MAX_BPS / 40) as usize, "{sent}");
        std::thread::sleep(Duration::from_millis(100));
        let mut more = 0usize;
        while b.take(1200) {
            more += 1200;
        }
        // Em 100 ms, ~125 KB a 10 Mb/s.
        assert!((100_000..160_000).contains(&more), "{more}");
    }
}
