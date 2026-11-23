//! Encontro direto (docs/encontro-direto.md): o pedido do espectador e a resposta do host
//! vão direto entre os dois apps, pelo socket da fofoca, em vez de passar pela DHT.
//! Corre junto com o encontro pela DHT; vale o que chegar primeiro.
//!
//! 1. O host deixa na DHT um *ticket* (cifrado com a chave do código) com os endereços
//!    onde pode ser chamado, e o atualiza quando eles mudam. Ler da DHT é bem mais
//!    confiável que gravar: o espectador só lê.
//! 2. O espectador manda o pedido direto para esses endereços, de segundo em segundo, e
//!    "bate à porta" pela fofoca: a batida corre de vizinho em vizinho até o host, que
//!    então chama o espectador de volta. Com os dois lados mandando, os roteadores deixam
//!    os pacotes passarem.
//! 3. O host responde direto para o endereço de onde o pedido veio (e também pela DHT).
//!
//! Tudo é cifrado com a chave do código (a mesma do encontro pela DHT): quem não tem o
//! código não lê nem forja nada, e a batida não mostra o endereço de quem bate.

use crate::gossip::{Gossip, Rv};
use crate::rendezvous::{Keys, OfferFn};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Pedaço de mensagem (bytes cifrados) por pacote: em base64 e com o resto do JSON, cabe
/// nos 1400 bytes da fofoca.
const PIECE: usize = 800;
const MAX_PIECES: u8 = 10;
/// O espectador manda o pedido de novo a cada tanto (pacotes UDP se perdem).
const RESEND: Duration = Duration::from_secs(1);
/// E bate à porta de novo a cada tanto (vizinhos mudam).
const KNOCK_EVERY: Duration = Duration::from_secs(5);
/// Pedaços incompletos são esquecidos depois disso.
const PARTIAL_TTL: Duration = Duration::from_secs(15);
/// Pedidos já vistos (para não entregar de novo o mesmo pedido repetido).
const SEEN_TTL: Duration = Duration::from_secs(300);

fn log(msg: impl AsRef<str>) {
    crate::connlog::log(format!("direto: {}", msg.as_ref()));
}

/// Onde chamar o host (o ticket, lido da DHT).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Ticket {
    pub addrs: Vec<String>,
    pub ts: i64,
}

impl Ticket {
    pub fn socket_addrs(&self) -> Vec<SocketAddr> {
        self.addrs.iter().filter_map(|a| a.parse().ok()).take(6).collect()
    }
}

struct HostReg {
    code: String,
    keys: Arc<Keys>,
    on_offer: OfferFn,
}

struct Waiter {
    keys: Arc<Keys>,
    tx: Sender<String>,
}

#[derive(Default)]
struct St {
    /// Transmissões deste app (como host), pela marca.
    hosts: HashMap<String, HostReg>,
    /// Entradas esperando resposta (como espectador), pela marca.
    waiters: HashMap<String, Waiter>,
    /// Pedaços chegando: (de onde, mensagem) -> (quando começou, pedaços).
    partial: HashMap<(SocketAddr, u32), (Instant, Vec<Option<Vec<u8>>>)>,
    /// Pedidos já entregues: "sid tipo" -> quando.
    seen: HashMap<String, Instant>,
    /// Último endereço de onde veio cada tentativa de entrada (sid).
    sid_addr: HashMap<String, (SocketAddr, String)>,
    /// Última resposta de cada tentativa (para mandar de novo se o pedido repetir).
    answers: HashMap<String, String>,
}

pub struct Direct {
    g: Arc<Gossip>,
    st: Mutex<St>,
    next_msg: AtomicU32,
}

static GLOBAL: OnceLock<Arc<Direct>> = OnceLock::new();

/// Liga o encontro direto à fofoca do app.
pub fn install(g: Arc<Gossip>) {
    let _ = GLOBAL.set(Direct::new(g));
}

pub fn global() -> Option<Arc<Direct>> {
    GLOBAL.get().cloned()
}

impl Direct {
    pub fn new(g: Arc<Gossip>) -> Arc<Direct> {
        let mut seed = [0u8; 4];
        let _ = getrandom::fill(&mut seed);
        let d = Arc::new(Direct { g: g.clone(), st: Mutex::new(St::default()), next_msg: AtomicU32::new(u32::from_le_bytes(seed)) });
        let w = Arc::downgrade(&d);
        g.on_rv(Box::new(move |from, rv| {
            if let Some(d) = w.upgrade() {
                d.on_rv(from, rv);
            }
        }));
        let w = Arc::downgrade(&d);
        g.on_knock(Box::new(move |k, e| w.upgrade().is_some_and(|d| d.on_knock(k, e))));
        d
    }

    /// Endereços onde este app pode ser chamado (para o ticket).
    pub fn my_addrs(&self) -> Vec<SocketAddr> {
        self.g.my_addrs()
    }

    /* ----------------------------- host ----------------------------- */

    /// Passa a atender pedidos diretos para este código.
    pub fn host_register(&self, code: &str, keys: Arc<Keys>, on_offer: OfferFn) {
        let tag = keys.direct_tag();
        self.st.lock().unwrap().hosts.insert(tag, HostReg { code: code.to_string(), keys, on_offer });
    }

    /// Para de atender todos os códigos.
    pub fn host_clear(&self) {
        let mut st = self.st.lock().unwrap();
        st.hosts.clear();
        st.sid_addr.clear();
        st.answers.clear();
    }

    /// Manda a resposta direto, se o pedido desta tentativa veio direto. Não bloqueia.
    pub fn host_reply(&self, code: &str, payload: &str) -> bool {
        let Some(sid) = sid_of(payload) else { return false };
        let (to, keys, tag) = {
            let mut st = self.st.lock().unwrap();
            let Some((to, tag)) = st.sid_addr.get(&sid).cloned() else { return false };
            let Some(h) = st.hosts.get(&tag).filter(|h| h.code == code) else { return false };
            let keys = h.keys.clone();
            st.answers.insert(sid.clone(), payload.to_string());
            (to, keys, tag)
        };
        let ok = self.send_sealed(to, &keys, &tag, 'h', 0, payload);
        log(format!("resposta mandada para a tentativa {} ({})", crate::connlog::short(&sid), if ok { "ok" } else { "falhou" }));
        ok
    }

    fn on_knock(&self, k: &str, e: &str) -> bool {
        let keys = match self.st.lock().unwrap().hosts.get(k) {
            Some(h) => h.keys.clone(),
            None => return false,
        };
        let Some(addrs) = B64.decode(e).ok().and_then(|b| keys.open(&b)) else { return true };
        let addrs: Vec<SocketAddr> = serde_json::from_str::<Vec<String>>(&addrs).unwrap_or_default().iter().filter_map(|a| a.parse().ok()).take(4).collect();
        log(format!("batida recebida; chamando {} endereço(s)", addrs.len()));
        // Chama de volta algumas vezes: abre a passagem pelo nosso roteador para o
        // espectador (que já está mandando o pedido para nós).
        let (g, k) = (self.g.clone(), k.to_string());
        thread::spawn(move || {
            for _ in 0..4 {
                for a in &addrs {
                    g.send_rv(*a, &Rv { k: k.clone(), r: 'h', s: 0, m: 0, i: 0, n: 0, d: String::new() });
                }
                thread::sleep(Duration::from_millis(250));
            }
        });
        true
    }

    /* --------------------------- espectador --------------------------- */

    /// Começa a mandar o pedido direto ao host (em segundo plano, até `done`). Cada
    /// mensagem do host (aberta) vai para `tx`.
    pub fn viewer_start(self: &Arc<Self>, keys: Arc<Keys>, slot: u8, payload: String, ticket: Option<Ticket>, tx: Sender<String>, done: Arc<AtomicBool>) {
        let tag = keys.direct_tag();
        self.st.lock().unwrap().waiters.insert(tag.clone(), Waiter { keys: keys.clone(), tx });
        let d = self.clone();
        thread::spawn(move || {
            let targets = ticket.map(|t| t.socket_addrs()).unwrap_or_default();
            let me: Vec<String> = d.g.my_addrs().iter().map(|a| a.to_string()).collect();
            let knock = serde_json::to_string(&me).ok().and_then(|s| keys.seal(&s).ok()).map(|b| B64.encode(b));
            log(format!("mandando o pedido direto para {} endereço(s); batida {}", targets.len(), if knock.is_some() && !me.is_empty() { "sim" } else { "não" }));
            let msg = d.next_msg.fetch_add(1, Ordering::SeqCst);
            let mut last_knock: Option<Instant> = None;
            while !done.load(Ordering::SeqCst) {
                if let Some(e) = knock.as_ref().filter(|_| !me.is_empty()) {
                    if last_knock.is_none_or(|t| t.elapsed() >= KNOCK_EVERY) {
                        last_knock = Some(Instant::now());
                        d.g.knock(&tag, e);
                    }
                }
                for a in &targets {
                    d.send_sealed_msg(*a, &keys, &tag, 'v', slot, msg, &payload);
                }
                let until = Instant::now() + RESEND;
                while Instant::now() < until && !done.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(50));
                }
            }
            d.st.lock().unwrap().waiters.remove(&tag);
        });
    }

    /// Antes do clique: bate à porta do host e manda um "olá" para os endereços do ticket.
    /// Abre a passagem pelos dois roteadores, e o pedido de verdade passa na hora.
    pub fn warm(&self, keys: &Keys, ticket: &Ticket) {
        let tag = keys.direct_tag();
        let me: Vec<String> = self.g.my_addrs().iter().map(|a| a.to_string()).collect();
        if !me.is_empty() {
            if let Some(e) = serde_json::to_string(&me).ok().and_then(|s| keys.seal(&s).ok()) {
                self.g.knock(&tag, &B64.encode(e));
            }
        }
        for a in ticket.socket_addrs() {
            self.g.send_rv(a, &Rv { k: tag.clone(), r: 'v', s: 0, m: 0, i: 0, n: 0, d: String::new() });
        }
    }

    /// Manda uma mensagem avulsa ao host (ex.: "desisti"), se houver para onde.
    pub fn viewer_send(&self, keys: &Keys, slot: u8, payload: &str, ticket: Option<&Ticket>) {
        let tag = keys.direct_tag();
        for a in ticket.map(|t| t.socket_addrs()).unwrap_or_default() {
            self.send_sealed(a, keys, &tag, 'v', slot, payload);
        }
    }

    /* ------------------------------ comum ------------------------------ */

    fn send_sealed(&self, to: SocketAddr, keys: &Keys, tag: &str, role: char, slot: u8, payload: &str) -> bool {
        let msg = self.next_msg.fetch_add(1, Ordering::SeqCst);
        self.send_sealed_msg(to, keys, tag, role, slot, msg, payload)
    }

    #[allow(clippy::too_many_arguments)]
    fn send_sealed_msg(&self, to: SocketAddr, keys: &Keys, tag: &str, role: char, slot: u8, msg: u32, payload: &str) -> bool {
        let Ok(sealed) = keys.seal(payload) else { return false };
        let pieces: Vec<&[u8]> = sealed.chunks(PIECE).collect();
        if pieces.len() > MAX_PIECES as usize {
            return false;
        }
        let n = pieces.len() as u8;
        pieces.iter().enumerate().all(|(i, p)| self.g.send_rv(to, &Rv { k: tag.to_string(), r: role, s: slot, m: msg, i: i as u8, n, d: B64.encode(p) }))
    }

    fn on_rv(&self, from: SocketAddr, rv: Rv) {
        if rv.n == 0 || rv.n > MAX_PIECES || rv.i >= rv.n {
            return; // só abriu a passagem pelo roteador
        }
        let Ok(piece) = B64.decode(rv.d.as_bytes()) else { return };
        if piece.len() > PIECE {
            return;
        }
        // Para quem é? (host: pedido de espectador; espectador: resposta do host)
        let keys = {
            let st = self.st.lock().unwrap();
            match rv.r {
                'v' => st.hosts.get(&rv.k).map(|h| h.keys.clone()),
                'h' => st.waiters.get(&rv.k).map(|w| w.keys.clone()),
                _ => None,
            }
        };
        let Some(keys) = keys else { return };
        let Some(sealed) = self.assemble(from, &rv, piece) else { return };
        let Some(text) = keys.open(&sealed) else { return };
        match rv.r {
            'v' => self.on_viewer_msg(from, &rv, text),
            _ => {
                if let Some(w) = self.st.lock().unwrap().waiters.get(&rv.k) {
                    let _ = w.tx.send(text);
                }
            }
        }
    }

    /// Junta os pedaços; devolve a mensagem quando estiver completa.
    fn assemble(&self, from: SocketAddr, rv: &Rv, piece: Vec<u8>) -> Option<Vec<u8>> {
        if rv.n == 1 {
            return Some(piece);
        }
        let mut st = self.st.lock().unwrap();
        st.partial.retain(|_, (at, _)| at.elapsed() < PARTIAL_TTL);
        if st.partial.len() > 64 {
            return None;
        }
        let entry = st.partial.entry((from, rv.m)).or_insert_with(|| (Instant::now(), vec![None; rv.n as usize]));
        if entry.1.len() != rv.n as usize {
            return None;
        }
        entry.1[rv.i as usize] = Some(piece);
        if entry.1.iter().all(|p| p.is_some()) {
            let (_, pieces) = st.partial.remove(&(from, rv.m))?;
            return Some(pieces.into_iter().flatten().flatten().collect());
        }
        None
    }

    /// Host: pedido (ou aviso) de um espectador, já aberto.
    fn on_viewer_msg(&self, from: SocketAddr, rv: &Rv, text: String) {
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        let Some(sid) = v["sid"].as_str().map(str::to_string) else { return };
        let kind = v["t"].as_str().unwrap_or("").to_string();
        let (first, resend, reg) = {
            let mut st = self.st.lock().unwrap();
            st.seen.retain(|_, at| at.elapsed() < SEEN_TTL);
            st.sid_addr.insert(sid.clone(), (from, rv.k.clone()));
            let first = st.seen.insert(format!("{sid} {kind}"), Instant::now()).is_none();
            let resend = (!first && kind == "offer").then(|| st.answers.get(&sid).cloned()).flatten();
            let reg = st.hosts.get(&rv.k).map(|h| (h.code.clone(), h.keys.clone(), h.on_offer.clone()));
            (first, resend, reg)
        };
        let Some((code, keys, on_offer)) = reg else { return };
        if first {
            log(format!("pedido direto ({kind}) da tentativa {}", crate::connlog::short(&sid)));
            on_offer(&code, rv.s % crate::rendezvous::SLOTS, text);
        } else if let Some(answer) = resend {
            // O espectador não recebeu a resposta: vai de novo (só direto; a DHT já tem).
            self.send_sealed(from, &keys, &rv.k, 'h', 0, &answer);
        }
    }
}

fn sid_of(payload: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(payload).ok()?["sid"].as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gossip::Config;
    use std::net::Ipv4Addr;
    use std::sync::mpsc;

    fn node(name: &str) -> Arc<Gossip> {
        Gossip::start(Config {
            id: name.into(),
            net: String::new(),
            dht: None,
            lan: false,
            bind: Ipv4Addr::LOCALHOST,
            check: Box::new(|_| None),
            deliver: Box::new(|_| {}),
        })
        .unwrap()
    }

    /// Pedido e resposta direto entre dois apps (sem DHT), com o pedido grande o bastante
    /// para ir em vários pedaços; o pedido repetido não é entregue de novo, mas a resposta
    /// volta de novo.
    #[test]
    fn offer_and_answer_go_direct() {
        let (hg, vg) = (node("host"), node("viewer"));
        let (host, viewer) = (Direct::new(hg.clone()), Direct::new(vg.clone()));
        let (code, keys) = Keys::new_host(None).unwrap();
        let keys = Arc::new(keys);
        let (otx, orx) = mpsc::channel::<(u8, String)>();
        let otx = Mutex::new(otx);
        host.host_register(&code, keys.clone(), Arc::new(move |_, slot, p| {
            let _ = otx.lock().unwrap().send((slot, p));
        }));

        let vkeys = Arc::new(Keys::from_code(&code, None).unwrap());
        let ticket = Ticket { addrs: hg.my_addrs().iter().map(|a| a.to_string()).collect(), ts: 0 };
        // SDP de verdade é ~2 KB e comprime mal: bytes aleatórios para dar vários pedaços.
        let mut noise = [0u8; 1500];
        getrandom::fill(&mut noise).unwrap();
        let offer = serde_json::json!({ "t": "offer", "sid": "abc123", "sdp": B64.encode(noise) }).to_string();
        let (atx, arx) = mpsc::channel();
        let done = Arc::new(AtomicBool::new(false));
        viewer.viewer_start(vkeys.clone(), 5, offer.clone(), Some(ticket), atx, done.clone());

        let (slot, got) = orx.recv_timeout(Duration::from_secs(3)).expect("pedido não chegou");
        assert_eq!((slot, got.as_str()), (5, offer.as_str()));
        let answer = serde_json::json!({ "t": "answer", "sid": "abc123", "sdp": "x" }).to_string();
        assert!(host.host_reply(&code, &answer));
        assert_eq!(arx.recv_timeout(Duration::from_secs(3)).expect("resposta não chegou"), answer);
        // O espectador continua mandando (até `done`): o host não entrega de novo...
        assert!(orx.recv_timeout(Duration::from_millis(1500)).is_err(), "pedido repetido entregue de novo");
        // ...mas manda a resposta de novo.
        assert_eq!(arx.recv_timeout(Duration::from_secs(3)).unwrap(), answer);
        done.store(true, Ordering::SeqCst);

        // Outro código (ou senha errada): o host nem vê.
        let (other, _) = Keys::new_host(None).unwrap();
        let okeys = Keys::from_code(&other, None).unwrap();
        viewer.viewer_send(&okeys, 0, &offer, Some(&Ticket { addrs: hg.my_addrs().iter().map(|a| a.to_string()).collect(), ts: 0 }));
        assert!(orx.recv_timeout(Duration::from_millis(500)).is_err());
        hg.stop();
        vg.stop();
    }

    /// O encontro direto também vai por IPv6 (aqui pelo ::1 do próprio PC).
    #[test]
    fn offer_and_answer_over_ipv6() {
        let mk = |name: &str| {
            Gossip::start(Config {
                id: name.into(),
                net: "teste-v6".into(),
                dht: None,
                lan: false,
                bind: Ipv4Addr::UNSPECIFIED,
                check: Box::new(|_| None),
                deliver: Box::new(|_| {}),
            })
            .unwrap()
        };
        let (hg, vg) = (mk("host6"), mk("viewer6"));
        let Some(port6) = hg.local_addr6().map(|a| a.port()) else {
            println!("sem IPv6 neste PC");
            return;
        };
        let (host, viewer) = (Direct::new(hg.clone()), Direct::new(vg.clone()));
        let (code, keys) = Keys::new_host(None).unwrap();
        let (otx, orx) = mpsc::channel::<String>();
        let otx = Mutex::new(otx);
        host.host_register(&code, Arc::new(keys), Arc::new(move |_, _, p| {
            let _ = otx.lock().unwrap().send(p);
        }));
        let ticket = Ticket { addrs: vec![format!("[::1]:{port6}")], ts: 0 };
        let offer = r#"{"t":"offer","sid":"v6"}"#.to_string();
        let (atx, arx) = mpsc::channel();
        let done = Arc::new(AtomicBool::new(false));
        viewer.viewer_start(Arc::new(Keys::from_code(&code, None).unwrap()), 1, offer.clone(), Some(ticket), atx, done.clone());
        assert_eq!(orx.recv_timeout(Duration::from_secs(3)).expect("pedido não chegou por IPv6"), offer);
        let answer = r#"{"t":"answer","sid":"v6"}"#;
        assert!(host.host_reply(&code, answer));
        assert_eq!(arx.recv_timeout(Duration::from_secs(3)).expect("resposta não voltou por IPv6"), answer);
        done.store(true, Ordering::SeqCst);
        hg.stop();
        vg.stop();
    }

    /// A batida corre pela fofoca (espectador — vizinho — host) e o host chama de volta.
    #[test]
    fn knock_reaches_the_host_through_a_neighbour() {
        let (hg, mg, vg) = (node("host"), node("meio"), node("viewer"));
        hg.connect(mg.local_addr());
        vg.connect(mg.local_addr());
        let until = Instant::now() + Duration::from_secs(5);
        while hg.peers() < 1 || vg.peers() < 1 || mg.peers() < 2 {
            assert!(Instant::now() < until, "vizinhos não se confirmaram");
            thread::sleep(Duration::from_millis(20));
        }
        let host = Direct::new(hg.clone());
        let _meio = Direct::new(mg.clone());
        let (code, keys) = Keys::new_host(None).unwrap();
        host.host_register(&code, Arc::new(keys), Arc::new(|_, _, _| {}));

        // O espectador "ouve" os chamados de volta do host.
        let (tx, rx) = mpsc::channel::<SocketAddr>();
        let tx = Mutex::new(tx);
        vg.on_rv(Box::new(move |from, rv| {
            if rv.r == 'h' && rv.n == 0 {
                let _ = tx.lock().unwrap().send(from);
            }
        }));
        let vkeys = Keys::from_code(&code, None).unwrap();
        let me: Vec<String> = vg.my_addrs().iter().map(|a| a.to_string()).collect();
        let e = B64.encode(vkeys.seal(&serde_json::to_string(&me).unwrap()).unwrap());
        vg.knock(&vkeys.direct_tag(), &e);
        let from = rx.recv_timeout(Duration::from_secs(3)).expect("host não chamou de volta");
        assert_eq!(from, hg.local_addr());
        for g in [hg, mg, vg] {
            g.stop();
        }
    }
}
