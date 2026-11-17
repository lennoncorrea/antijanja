//! Fofoca entre os apps abertos (docs/gossip-ao-vivo.md): os anúncios do "Ao vivo agora"
//! chegam a todo mundo na hora, sem ninguém ficar consultando a DHT.
//!
//! Não sabe o que é um anúncio: quem usa diz como conferir (`Config::check`) e o que fazer
//! com um anúncio novo (`Config::deliver`). Os anúncios do mural são ligados aqui em
//! `rendezvous` (`start_gossip`).
//!
//! UDP, um pacote por mensagem: `AJG1` + JSON. Threads: recepção, manutenção (pings,
//! novas tentativas, troca de vizinhos), descoberta pela DHT (anuncia e busca o tópico) e
//! descoberta na rede local (multicast).

#![allow(deprecated)] // API bloqueante do mainline, em thread própria (como em `rendezvous`)

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const MAGIC: &[u8; 4] = b"AJG1";
const VERSION: u32 = 1;
/// Maior pacote (cabe sem fragmentar na maioria das redes).
const MAX_PACKET: usize = 1400;
/// Vizinhos que o app procura ter; acima de `MAX_PEERS` recusa novos.
const TARGET_PEERS: usize = 8;
const MAX_PEERS: usize = 16;
/// Ping para manter aberta a passagem pelo roteador (que costuma fechar em 30 s ou mais).
const PING_EVERY: Duration = Duration::from_secs(15);
const PEER_TIMEOUT: Duration = Duration::from_secs(50);
/// `hi` repetido uma vez por segundo, até tantas vezes (atravessar o roteador pede que os
/// dois lados mandem; um dos lados pode demorar a começar).
const HI_TRIES: u8 = 10;
/// Troca de vizinhos (apresentação) a cada tanto.
const PEX_EVERY: Duration = Duration::from_secs(30);
const MAX_HOPS: u8 = 8;
/// Anúncios já vistos (para não repassar de novo), guardados até tantos.
const SEEN_MAX: usize = 4096;
/// Mensagens por vizinho numa janela; acima disso, ignora.
const RATE_WINDOW: Duration = Duration::from_secs(10);
const RATE_MAX: u32 = 300;
/// Busca no tópico da DHT: com poucos vizinhos, a cada minuto; com bastante, a cada 5.
const LOOKUP_FEW: Duration = Duration::from_secs(60);
const LOOKUP_MANY: Duration = Duration::from_secs(300);
/// Anúncio no tópico (a DHT esquece em ~30 min) e novo STUN.
const ANNOUNCE_EVERY: Duration = Duration::from_secs(300);
const LAN_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 74, 74);
const LAN_PORT: u16 = 47474;
const LAN_EVERY: Duration = Duration::from_secs(30);
const STUN_SERVERS: [&str; 2] = ["stun.l.google.com:19302", "stun.cloudflare.com:3478"];
/// Anúncios guardados para quem chega (`sync`) por esse tempo.
const LIVE_TTL: Duration = Duration::from_secs(180);

/// Validação de um anúncio: `Some((chave, horário))` se valer. Entre anúncios com a mesma
/// chave, vale o de horário maior.
pub type CheckFn = Box<dyn Fn(&[u8]) -> Option<(String, i64)> + Send + Sync>;
/// Anúncio novo (já conferido).
pub type DeliverFn = Box<dyn Fn(&[u8]) + Send + Sync>;

pub struct Config {
    /// Identificador desta execução (para não conectar em si mesmo).
    pub id: String,
    /// Rede: vazio = a de verdade; outro nome = rede de teste isolada (outro tópico, e só
    /// conversa com quem está na mesma).
    pub net: String,
    /// Descoberta pelo tópico na DHT (sem ela, só rede local e `connect`).
    pub dht: Option<mainline::Dht>,
    /// Descoberta na rede local por multicast.
    pub lan: bool,
    /// Endereço onde escutar (0.0.0.0; nos testes, 127.0.0.1).
    pub bind: Ipv4Addr,
    pub check: CheckFn,
    pub deliver: DeliverFn,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Msg {
    /// Quero conversar (`n`: a rede).
    Hi {
        id: String,
        v: u32,
        #[serde(default)]
        n: String,
    },
    /// Aceito (e é neste endereço que vejo você).
    Hey { id: String, v: u32, you: String },
    Ping,
    Pong,
    /// Vizinhos meus, para você chamar.
    Peers { list: Vec<String> },
    /// Fulano vai te chamar: chame ele também (os dois mandando, o roteador deixa passar).
    Meet { addr: String },
    /// Um anúncio (base64) e quantos saltos já deu.
    Live { e: String, h: u8 },
    /// Acabei de chegar: me mande os anúncios atuais.
    Sync,
    Bye,
    /// Aviso na rede local (multicast): estou nesta porta (`n`: a rede).
    Lan {
        id: String,
        port: u16,
        #[serde(default)]
        n: String,
    },
}

fn encode(m: &Msg) -> Option<Vec<u8>> {
    let mut out = MAGIC.to_vec();
    out.extend(serde_json::to_vec(m).ok()?);
    (out.len() <= MAX_PACKET).then_some(out)
}

fn decode(b: &[u8]) -> Option<Msg> {
    serde_json::from_slice(b.strip_prefix(MAGIC.as_slice())?).ok()
}

struct Peer {
    last_seen: Instant,
    last_ping: Instant,
    /// Provou o endereço (respondeu a um `hi` ou a um `ping` nosso): só esses recebem
    /// repasses e o `sync` (um pedido com endereço falso não faz o app mandar a lista
    /// para outro).
    verified: bool,
    rate: (Instant, u32),
}

#[derive(Default)]
struct State {
    peers: HashMap<SocketAddr, Peer>,
    /// `hi` enviados esperando resposta: (último envio, tentativas).
    pending: HashMap<SocketAddr, (Instant, u8)>,
    seen: HashSet<[u8; 32]>,
    seen_order: VecDeque<[u8; 32]>,
    /// Anúncio mais novo de cada chave: (horário, quando chegou, bytes).
    live: HashMap<String, (i64, Instant, Vec<u8>)>,
    /// Endereço público (STUN ou o que os vizinhos dizem ver).
    public: Option<SocketAddrV4>,
    /// Transação do último pedido de STUN.
    stun_tx: Option<[u8; 12]>,
    last_pex: Option<Instant>,
    pex_turn: usize,
}

pub struct Gossip {
    sock: UdpSocket,
    cfg: Config,
    st: Mutex<State>,
    stop: AtomicBool,
}

static GLOBAL: OnceLock<Arc<Gossip>> = OnceLock::new();

/// Guarda a fofoca do app (para `broadcast` e `peer_count` de qualquer lugar).
pub fn install(g: Arc<Gossip>) {
    let _ = GLOBAL.set(g);
}

/// Espalha um anúncio pela fofoca do app (se ela estiver rodando).
pub fn broadcast(bytes: &[u8]) {
    if let Some(g) = GLOBAL.get() {
        g.announce(bytes);
    }
}

/// Vizinhos confirmados na fofoca do app.
pub fn peer_count() -> usize {
    GLOBAL.get().map(|g| g.peers()).unwrap_or(0)
}

fn log(msg: impl AsRef<str>) {
    crate::connlog::log(format!("fofoca: {}", msg.as_ref()));
}

impl Gossip {
    pub fn start(cfg: Config) -> std::io::Result<Arc<Gossip>> {
        let sock = UdpSocket::bind(SocketAddrV4::new(cfg.bind, 0))?;
        sock.set_read_timeout(Some(Duration::from_millis(500)))?;
        let lan = cfg.lan;
        let dht = cfg.dht.clone();
        let g = Arc::new(Gossip { sock, cfg, st: Mutex::new(State::default()), stop: AtomicBool::new(false) });
        log(format!("escutando na porta {}", g.local_addr().port()));
        let spawn = |name: &str, f: fn(Arc<Gossip>)| {
            let g = g.clone();
            thread::Builder::new().name(name.into()).spawn(move || f(g))
        };
        spawn("aj-gossip-rx", Gossip::recv_loop)?;
        spawn("aj-gossip-tick", Gossip::tick_loop)?;
        if dht.is_some() {
            spawn("aj-gossip-dht", Gossip::discover_loop)?;
        }
        if lan {
            spawn("aj-gossip-lan", Gossip::lan_loop)?;
        }
        Ok(g)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.sock.local_addr().unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)))
    }

    /// Vizinhos confirmados.
    pub fn peers(&self) -> usize {
        self.st.lock().unwrap().peers.values().filter(|p| p.verified).count()
    }

    /// Chama um app neste endereço.
    pub fn connect(&self, addr: SocketAddr) {
        let mut st = self.st.lock().unwrap();
        if self.want(&st, addr) {
            st.pending.insert(addr, (Instant::now(), 1));
            drop(st);
            self.send(addr, &self.hi());
        }
    }

    fn hi(&self) -> Msg {
        Msg::Hi { id: self.cfg.id.clone(), v: VERSION, n: self.cfg.net.clone() }
    }

    /// Para: avisa os vizinhos e encerra as threads.
    pub fn stop(&self) {
        let peers: Vec<SocketAddr> = self.st.lock().unwrap().peers.keys().copied().collect();
        for p in peers {
            self.send(p, &Msg::Bye);
        }
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Espalha um anúncio nosso (e entrega aqui também).
    pub fn announce(&self, bytes: &[u8]) {
        let Some(e) = self.accept(bytes) else { return };
        (self.cfg.deliver)(bytes);
        let to: Vec<SocketAddr> = self.st.lock().unwrap().peers.iter().filter(|(_, p)| p.verified).map(|(a, _)| *a).collect();
        let msg = Msg::Live { e, h: 0 };
        for a in to {
            self.send(a, &msg);
        }
    }

    /// Anúncio novo e válido? Guarda e devolve em base64; repetido ou inválido, `None`.
    fn accept(&self, bytes: &[u8]) -> Option<String> {
        let hash = *blake3::hash(bytes).as_bytes();
        {
            let mut st = self.st.lock().unwrap();
            if !st.seen.insert(hash) {
                return None;
            }
            st.seen_order.push_back(hash);
            while st.seen_order.len() > SEEN_MAX {
                if let Some(old) = st.seen_order.pop_front() {
                    st.seen.remove(&old);
                }
            }
        }
        let (key, ts) = (self.cfg.check)(bytes)?;
        let mut st = self.st.lock().unwrap();
        if st.live.get(&key).is_some_and(|(old, _, _)| *old >= ts) {
            return None; // mais velho que o que já temos
        }
        st.live.insert(key, (ts, Instant::now(), bytes.to_vec()));
        Some(B64.encode(bytes))
    }

    /// Vale chamar `addr`? (não somos nós, não é vizinho nem está pendente, e há vaga)
    fn want(&self, st: &State, addr: SocketAddr) -> bool {
        let me = self.local_addr();
        let own = |a: SocketAddr| a == me || st.public.is_some_and(|p| SocketAddr::V4(p) == a) || (a.ip().is_loopback() && a.port() == me.port());
        !own(addr) && !st.peers.contains_key(&addr) && !st.pending.contains_key(&addr) && st.peers.len() + st.pending.len() < MAX_PEERS && addr.port() != 0
    }

    fn send(&self, to: SocketAddr, m: &Msg) {
        if let Some(b) = encode(m) {
            let _ = self.sock.send_to(&b, to);
        }
    }

    fn recv_loop(self: Arc<Self>) {
        let mut buf = [0u8; 2048];
        while !self.stop.load(Ordering::SeqCst) {
            match self.sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let data = &buf[..n];
                    if let Some(m) = decode(data) {
                        self.handle(from, m);
                    } else if let Some(addr) = self.stun_reply(data) {
                        let mut st = self.st.lock().unwrap();
                        if st.public != Some(addr) {
                            st.public = Some(addr);
                            drop(st);
                            log(format!("endereço público {addr} (STUN)"));
                        }
                    }
                }
                // Tempo esgotado, ou (no Windows) "porta inalcançável" de um envio anterior.
                Err(_) => continue,
            }
        }
    }

    fn handle(&self, from: SocketAddr, m: Msg) {
        let now = Instant::now();
        let mut out: Vec<(SocketAddr, Msg)> = Vec::new();
        let mut deliver: Option<Vec<u8>> = None;
        {
            let mut st = self.st.lock().unwrap();
            // Limite por vizinho.
            if let Some(p) = st.peers.get_mut(&from) {
                if p.rate.0.elapsed() > RATE_WINDOW {
                    p.rate = (now, 0);
                }
                p.rate.1 += 1;
                if p.rate.1 > RATE_MAX {
                    return;
                }
                p.last_seen = now;
            }
            let verified = st.peers.get(&from).is_some_and(|p| p.verified);
            match m {
                Msg::Hi { id, v, n } => {
                    if id == self.cfg.id || v != VERSION || n != self.cfg.net || (!st.peers.contains_key(&from) && st.peers.len() >= MAX_PEERS) {
                        return;
                    }
                    st.pending.remove(&from);
                    let new = !st.peers.contains_key(&from);
                    st.peers.entry(from).or_insert(Peer { last_seen: now, last_ping: now, verified: false, rate: (now, 1) });
                    if new {
                        log(format!("vizinho novo {from} (chamou)"));
                    }
                    out.push((from, Msg::Hey { id: self.cfg.id.clone(), v: VERSION, you: from.to_string() }));
                    // Ping para ele provar o endereço (o `pong` confirma).
                    out.push((from, Msg::Ping));
                }
                Msg::Hey { id, v, you } => {
                    // Só vale como resposta a um `hi` nosso.
                    if id == self.cfg.id || v != VERSION || !(st.pending.contains_key(&from) || st.peers.contains_key(&from)) {
                        return;
                    }
                    st.pending.remove(&from);
                    let was = st.peers.get(&from).is_some_and(|p| p.verified);
                    st.peers.entry(from).or_insert(Peer { last_seen: now, last_ping: now, verified: false, rate: (now, 1) }).verified = true;
                    if st.public.is_none() && is_global(from.ip()) {
                        if let Ok(SocketAddr::V4(a)) = you.parse::<SocketAddr>() {
                            st.public = Some(a);
                        }
                    }
                    if !was {
                        log(format!("vizinho confirmado {from} ({} no total)", st.peers.values().filter(|p| p.verified).count()));
                        out.extend(Self::snapshot(&st, from));
                        out.push((from, Msg::Sync));
                    }
                }
                Msg::Ping => {
                    if st.peers.contains_key(&from) {
                        out.push((from, Msg::Pong));
                    }
                }
                Msg::Pong => {
                    if let Some(p) = st.peers.get_mut(&from) {
                        if !p.verified {
                            p.verified = true;
                            // O `sync` dele pode ter chegado antes deste `pong` (e sido
                            // ignorado): manda já o que temos, e pede o que ele tem.
                            out.extend(Self::snapshot(&st, from));
                            out.push((from, Msg::Sync));
                            let n = st.peers.values().filter(|p| p.verified).count();
                            log(format!("vizinho confirmado {from} ({n} no total)"));
                        }
                    }
                }
                Msg::Peers { list } if verified => {
                    for a in list.iter().take(MAX_PEERS).filter_map(|s| s.parse::<SocketAddr>().ok()) {
                        if self.want(&st, a) {
                            st.pending.insert(a, (now, 1));
                            out.push((a, self.hi()));
                        }
                    }
                }
                Msg::Meet { addr } if verified => {
                    if let Ok(a) = addr.parse::<SocketAddr>() {
                        if self.want(&st, a) {
                            st.pending.insert(a, (now, 1));
                            out.push((a, self.hi()));
                        }
                    }
                }
                Msg::Live { e, h } if st.peers.contains_key(&from) => {
                    drop(st);
                    let Ok(bytes) = B64.decode(e.as_bytes()) else { return };
                    if bytes.len() > MAX_PACKET {
                        return;
                    }
                    let Some(e) = self.accept(&bytes) else { return };
                    deliver = Some(bytes);
                    if h < MAX_HOPS {
                        let st = self.st.lock().unwrap();
                        for (a, p) in &st.peers {
                            if *a != from && p.verified {
                                out.push((*a, Msg::Live { e: e.clone(), h: h + 1 }));
                            }
                        }
                    }
                    // `st` já foi solto: o resto é fora do bloco.
                    return self.flush(out, deliver);
                }
                Msg::Sync if verified => out.extend(Self::snapshot(&st, from)),
                Msg::Bye => {
                    if st.peers.remove(&from).is_some() {
                        log(format!("vizinho saiu {from}"));
                    }
                }
                _ => {}
            }
        }
        self.flush(out, deliver);
    }

    /// Os anúncios atuais, para mandar a `to`.
    fn snapshot(st: &State, to: SocketAddr) -> Vec<(SocketAddr, Msg)> {
        st.live.values().filter(|(_, at, _)| at.elapsed() < LIVE_TTL).map(|(_, _, b)| (to, Msg::Live { e: B64.encode(b), h: 0 })).collect()
    }

    fn flush(&self, out: Vec<(SocketAddr, Msg)>, deliver: Option<Vec<u8>>) {
        for (a, m) in out {
            self.send(a, &m);
        }
        if let Some(b) = deliver {
            (self.cfg.deliver)(&b);
        }
    }

    fn tick_loop(self: Arc<Self>) {
        while !self.stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(500));
            let now = Instant::now();
            let mut out: Vec<(SocketAddr, Msg)> = Vec::new();
            {
                let mut st = self.st.lock().unwrap();
                st.peers.retain(|a, p| {
                    let alive = p.last_seen.elapsed() < PEER_TIMEOUT;
                    if !alive {
                        log(format!("vizinho sumiu {a}"));
                    }
                    alive
                });
                for (a, p) in st.peers.iter_mut() {
                    if p.last_ping.elapsed() >= PING_EVERY {
                        p.last_ping = now;
                        out.push((*a, Msg::Ping));
                    }
                }
                let hi = self.hi();
                st.pending.retain(|a, (at, n)| {
                    if at.elapsed() < Duration::from_secs(1) {
                        return true;
                    }
                    if *n >= HI_TRIES {
                        return false;
                    }
                    *at = now;
                    *n += 1;
                    out.push((*a, hi.clone()));
                    true
                });
                st.live.retain(|_, (_, at, _)| at.elapsed() < LIVE_TTL);
                // Apresenta um vizinho aos outros (os dois lados chamam ao mesmo tempo).
                if st.last_pex.is_none_or(|t| t.elapsed() >= PEX_EVERY) {
                    st.last_pex = Some(now);
                    let verified: Vec<SocketAddr> = st.peers.iter().filter(|(_, p)| p.verified).map(|(a, _)| *a).collect();
                    if verified.len() >= 2 {
                        st.pex_turn = st.pex_turn.wrapping_add(1);
                        let a = verified[st.pex_turn % verified.len()];
                        let others: Vec<SocketAddr> = verified.iter().copied().filter(|x| *x != a).take(8).collect();
                        out.push((a, Msg::Peers { list: others.iter().map(|x| x.to_string()).collect() }));
                        for c in others {
                            out.push((c, Msg::Meet { addr: a.to_string() }));
                        }
                    }
                }
            }
            self.flush(out, None);
        }
    }

    /// Descoberta pelo tópico do AntiJanja na DHT.
    fn discover_loop(self: Arc<Self>) {
        let Some(dht) = self.cfg.dht.clone() else { return };
        let topic = topic_id(&self.cfg.net);
        let mut last_announce: Option<Instant> = None;
        while !self.stop.load(Ordering::SeqCst) {
            if last_announce.is_none_or(|t| t.elapsed() >= ANNOUNCE_EVERY) {
                self.stun();
                // Espera a resposta do STUN chegar pela thread de recepção.
                for _ in 0..20 {
                    if self.st.lock().unwrap().public.is_some() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                let port = self.st.lock().unwrap().public.map(|p| p.port()).unwrap_or(self.local_addr().port());
                match dht.announce_peer(topic, Some(port)) {
                    Ok(_) => log(format!("anunciado no tópico (porta {port})")),
                    Err(e) => log(format!("falha ao anunciar no tópico: {e}")),
                }
                last_announce = Some(Instant::now());
            }
            let public_ip = self.st.lock().unwrap().public.map(|p| *p.ip());
            let mut found = 0;
            for batch in dht.get_peers(topic) {
                for a in batch {
                    // Mesmo IP público = mesmo roteador (ou este PC): pela rede local.
                    if Some(*a.ip()) == public_ip {
                        continue;
                    }
                    found += 1;
                    self.connect(SocketAddr::V4(a));
                }
            }
            let peers = self.peers();
            log(format!("busca no tópico: {found} apps encontrados, {peers} vizinhos"));
            let wait = if peers < TARGET_PEERS { LOOKUP_FEW } else { LOOKUP_MANY };
            let until = Instant::now() + wait;
            while Instant::now() < until && !self.stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(500));
            }
        }
    }

    /// Pede o endereço público a um servidor STUN (a resposta chega em `recv_loop`).
    fn stun(&self) {
        let mut tx = [0u8; 12];
        let _ = getrandom::fill(&mut tx);
        self.st.lock().unwrap().stun_tx = Some(tx);
        let req = stun_request(&tx);
        for s in STUN_SERVERS {
            if let Some(a) = s.to_socket_addrs().ok().and_then(|mut it| it.find(|a| a.is_ipv4())) {
                let _ = self.sock.send_to(&req, a);
            }
        }
    }

    fn stun_reply(&self, data: &[u8]) -> Option<SocketAddrV4> {
        let tx = self.st.lock().unwrap().stun_tx?;
        parse_stun(data, &tx)
    }

    /// Descoberta na rede local: avisa por multicast e chama quem avisar.
    fn lan_loop(self: Arc<Self>) {
        use socket2::{Domain, Protocol, Socket, Type};
        let listen = (|| -> std::io::Result<UdpSocket> {
            let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            s.set_reuse_address(true)?;
            s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, LAN_PORT)).into())?;
            s.join_multicast_v4(&LAN_GROUP, &Ipv4Addr::UNSPECIFIED)?;
            s.set_read_timeout(Some(Duration::from_millis(500)))?;
            Ok(s.into())
        })();
        let listen = match listen {
            Ok(s) => s,
            Err(e) => return log(format!("sem rede local: {e}")),
        };
        let beacon = Msg::Lan { id: self.cfg.id.clone(), port: self.local_addr().port(), n: self.cfg.net.clone() };
        let group = SocketAddr::from((LAN_GROUP, LAN_PORT));
        let mut last: Option<Instant> = None;
        let mut buf = [0u8; 512];
        while !self.stop.load(Ordering::SeqCst) {
            if last.is_none_or(|t| t.elapsed() >= LAN_EVERY) {
                last = Some(Instant::now());
                self.send(group, &beacon);
            }
            if let Ok((n, from)) = listen.recv_from(&mut buf) {
                if let Some(Msg::Lan { id, port, n: net }) = decode(&buf[..n]) {
                    if id != self.cfg.id && net == self.cfg.net {
                        self.connect(SocketAddr::new(from.ip(), port));
                    }
                }
            }
        }
    }
}

/// Tópico do AntiJanja na DHT (20 bytes, como um infohash); outro por rede de teste.
fn topic_id(net: &str) -> mainline::Id {
    let h = blake3::hash(format!("antijanja gossip v1{net}").as_bytes());
    mainline::Id::from_bytes(&h.as_bytes()[..20]).expect("20 bytes")
}

fn is_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => !(v.is_private() || v.is_loopback() || v.is_link_local() || v.is_unspecified() || v.octets()[0] == 100 && (v.octets()[1] & 0xC0) == 64),
        IpAddr::V6(_) => false,
    }
}

const STUN_COOKIE: u32 = 0x2112_A442;

/// Pedido de endereço (Binding Request, RFC 5389).
fn stun_request(tx: &[u8; 12]) -> Vec<u8> {
    let mut b = vec![0x00, 0x01, 0x00, 0x00];
    b.extend(STUN_COOKIE.to_be_bytes());
    b.extend(tx);
    b
}

/// Endereço na resposta do STUN (XOR-MAPPED-ADDRESS, ou MAPPED-ADDRESS).
fn parse_stun(d: &[u8], tx: &[u8; 12]) -> Option<SocketAddrV4> {
    if d.len() < 20 || d[0..2] != [0x01, 0x01] || d[4..8] != STUN_COOKIE.to_be_bytes() || &d[8..20] != tx {
        return None;
    }
    let len = u16::from_be_bytes([d[2], d[3]]) as usize;
    let body = d.get(20..20 + len)?;
    let mut i = 0;
    let mut plain = None;
    while i + 4 <= body.len() {
        let kind = u16::from_be_bytes([body[i], body[i + 1]]);
        let alen = u16::from_be_bytes([body[i + 2], body[i + 3]]) as usize;
        let v = body.get(i + 4..i + 4 + alen)?;
        if v.len() >= 8 && v[1] == 0x01 {
            let port = u16::from_be_bytes([v[2], v[3]]);
            let ip = [v[4], v[5], v[6], v[7]];
            match kind {
                0x0020 => {
                    let c = STUN_COOKIE.to_be_bytes();
                    let port = port ^ (STUN_COOKIE >> 16) as u16;
                    let ip = Ipv4Addr::new(ip[0] ^ c[0], ip[1] ^ c[1], ip[2] ^ c[2], ip[3] ^ c[3]);
                    return Some(SocketAddrV4::new(ip, port));
                }
                0x0001 => plain = Some(SocketAddrV4::new(Ipv4Addr::from(ip), port)),
                _ => {}
            }
        }
        i += 4 + alen.div_ceil(4) * 4;
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn messages_roundtrip_and_reject_garbage() {
        let m = Msg::Live { e: "abc".into(), h: 3 };
        assert_eq!(decode(&encode(&m).unwrap()), Some(m));
        assert_eq!(decode(b"AJG1{\"t\":\"desconhecida\"}"), None);
        assert_eq!(decode(b"XXXX{\"t\":\"ping\"}"), None);
        assert!(encode(&Msg::Live { e: "x".repeat(2000), h: 0 }).is_none(), "pacote grande demais");
    }

    #[test]
    fn parses_stun_xor_mapped_address() {
        let tx = [7u8; 12];
        // Resposta com XOR-MAPPED-ADDRESS de 203.0.113.5:40000.
        let c = STUN_COOKIE.to_be_bytes();
        let port = 40000u16 ^ (STUN_COOKIE >> 16) as u16;
        let ip = [203 ^ c[0], 0 ^ c[1], 113 ^ c[2], 5 ^ c[3]];
        let mut d = vec![0x01, 0x01, 0x00, 12];
        d.extend(c);
        d.extend(tx);
        d.extend([0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
        d.extend(port.to_be_bytes());
        d.extend(ip);
        assert_eq!(parse_stun(&d, &tx), Some("203.0.113.5:40000".parse().unwrap()));
        assert_eq!(parse_stun(&d, &[8u8; 12]), None, "transação de outro pedido");
    }

    /// Anúncio de teste: "chave:horário".
    fn check(b: &[u8]) -> Option<(String, i64)> {
        let s = std::str::from_utf8(b).ok()?;
        let (k, ts) = s.split_once(':')?;
        Some((k.to_string(), ts.parse().ok()?))
    }

    fn node(name: &str) -> (Arc<Gossip>, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let g = Gossip::start(Config {
            id: name.into(),
            net: String::new(),
            dht: None,
            lan: false,
            bind: Ipv4Addr::LOCALHOST,
            check: Box::new(check),
            deliver: Box::new(move |b| {
                let _ = tx.lock().unwrap().send(String::from_utf8_lossy(b).into_owned());
            }),
        })
        .unwrap();
        (g, rx)
    }

    fn wait_peers(g: &Gossip, n: usize) {
        let until = Instant::now() + Duration::from_secs(5);
        while g.peers() < n {
            assert!(Instant::now() < until, "vizinhos não se confirmaram");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// A — B — C em linha: o anúncio de A chega a C (repassado por B) em bem menos de 1 s,
    /// uma vez só; quem chega depois recebe pelo `sync`; anúncio mais velho não passa.
    #[test]
    fn announcements_spread_once_and_sync_newcomers() {
        let (a, _ra) = node("a");
        let (b, rb) = node("b");
        let (c, rc) = node("c");
        a.connect(b.local_addr());
        c.connect(b.local_addr());
        wait_peers(&b, 2);
        wait_peers(&a, 1);
        wait_peers(&c, 1);

        let t = Instant::now();
        a.announce(b"live1:100");
        assert_eq!(rc.recv_timeout(Duration::from_secs(2)).unwrap(), "live1:100");
        println!("chegou em C em {:?}", t.elapsed());
        assert!(t.elapsed() < Duration::from_millis(500));
        assert_eq!(rb.recv_timeout(Duration::from_secs(1)).unwrap(), "live1:100");
        assert!(rc.recv_timeout(Duration::from_millis(300)).is_err(), "C recebeu duas vezes");

        // Mais velho que o que já circulou: não passa.
        a.announce(b"live1:50");
        assert!(rc.recv_timeout(Duration::from_millis(300)).is_err(), "anúncio velho passou");
        // Inválido: não passa.
        a.announce(b"lixo");
        assert!(rc.recv_timeout(Duration::from_millis(300)).is_err());

        // D chega depois e pede o que está valendo.
        let (d, rd) = node("d");
        d.connect(c.local_addr());
        assert_eq!(rd.recv_timeout(Duration::from_secs(3)).unwrap(), "live1:100");

        // Quem sai avisa (a troca de vizinhos pode ter ligado C a A também: compara com antes).
        wait_peers(&d, 1);
        let before = c.peers();
        d.stop();
        thread::sleep(Duration::from_millis(300));
        assert_eq!(c.peers(), before - 1, "D saiu e devia ter avisado");
        for g in [a, b, c] {
            g.stop();
        }
    }

    /// Redes diferentes (a de verdade e uma de teste) não viram vizinhas.
    #[test]
    fn different_networks_do_not_peer() {
        let (a, _ra) = node("a");
        let (tx, _rx) = mpsc::channel::<String>();
        let tx = Mutex::new(tx);
        let t = Gossip::start(Config {
            id: "t".into(),
            net: "teste".into(),
            dht: None,
            lan: false,
            bind: Ipv4Addr::LOCALHOST,
            check: Box::new(check),
            deliver: Box::new(move |b| {
                let _ = tx.lock().unwrap().send(String::from_utf8_lossy(b).into_owned());
            }),
        })
        .unwrap();
        t.connect(a.local_addr());
        a.connect(t.local_addr());
        thread::sleep(Duration::from_millis(500));
        assert_eq!((a.peers(), t.peers()), (0, 0));
        a.stop();
        t.stop();
    }

    /// Apresentação: A conhece B, C conhece B; depois da troca de vizinhos, A e C ficam
    /// vizinhos diretos.
    #[test]
    fn peer_exchange_introduces_neighbours() {
        let (a, _ra) = node("a");
        let (b, _rb) = node("b");
        let (c, _rc) = node("c");
        a.connect(b.local_addr());
        c.connect(b.local_addr());
        wait_peers(&b, 2);
        // A troca de vizinhos acontece logo no começo (e depois a cada PEX_EVERY).
        b.st.lock().unwrap().last_pex = None;
        wait_peers(&a, 2);
        wait_peers(&c, 2);
        for g in [a, b, c] {
            g.stop();
        }
    }

    /// Dois apps na rede local (o mesmo PC) se acham pelo multicast. Pode falhar se o
    /// firewall bloquear multicast: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn lan_discovery() {
        let mk = |name: &str| {
            Gossip::start(Config {
                id: name.into(),
                net: String::new(),
                dht: None,
                lan: true,
                bind: Ipv4Addr::UNSPECIFIED,
                check: Box::new(check),
                deliver: Box::new(|_| {}),
            })
            .unwrap()
        };
        let (a, b) = (mk("a"), mk("b"));
        let until = Instant::now() + Duration::from_secs(10);
        while a.peers() < 1 || b.peers() < 1 {
            assert!(Instant::now() < until, "não se acharam pela rede local");
            thread::sleep(Duration::from_millis(100));
        }
        a.stop();
        b.stop();
    }
}
