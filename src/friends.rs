//! Amizades sem servidor (docs/amizades.md).
//!
//! - **Cartão da conta** na DHT (chave mestra, salt `card`): nome, chave de caixa e
//!   aparelhos desvinculados. O principal assina; os outros aparelhos republicam o mesmo
//!   item (a DHT esquece em ~2 h).
//! - **Pedido de amizade**: cifrado para a chave de caixa de quem recebe, numa das vagas da
//!   caixa de entrada dele (chave derivada do código de amigo: quem tem o código escreve).
//! - **Canal do par**: do Diffie-Hellman das duas chaves de caixa saem a cifra e uma chave de
//!   escrita para cada lado. Cada um publica ali a própria presença (online, endereço,
//!   live, convite); a primeira presença de quem aceitou é o "aceito".
//! - **Conexão direta**: com os endereços da presença, os dois lados mandam pacotes pelo
//!   socket da fofoca (atravessa o roteador) e daí em diante avisos e convites chegam na
//!   hora.
//!
//! Tudo que sai de um aparelho vai assinado por ele, com o certificado da conta.

#![allow(deprecated)] // API bloqueante do mainline, em threads próprias (como em `rendezvous`)

use crate::identity::{self, DeviceCert, DeviceSig, Identity};
use crate::rendezvous::now_ms;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use mainline::{Dht, MutableItem};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

/// Vagas da caixa de entrada de pedidos.
const INBOX_SLOTS: u8 = 16;
/// Maior valor que a DHT guarda (BEP44).
const MAX_VALUE: usize = 1000;
const MAX_PLAIN: u64 = 64 * 1024;
/// Presença republicada a cada tanto (e na hora, numa mudança).
const PRESENCE_EVERY: Duration = Duration::from_secs(4 * 60);
/// Presença mais velha que isso = offline.
const PRESENCE_FRESH_MS: i64 = 9 * 60 * 1000;
/// Leitura da presença dos amigos: sem conexão direta, a cada minuto; com, a cada 5.
const POLL_FEW: Duration = Duration::from_secs(60);
const POLL_LINKED: Duration = Duration::from_secs(5 * 60);
const INBOX_EVERY: Duration = Duration::from_secs(90);
const CARD_EVERY: Duration = Duration::from_secs(30 * 60);
const FRIEND_CARD_EVERY: Duration = Duration::from_secs(60 * 60);
/// Pedido enviado: republicado a cada tanto, por até tanto.
const REQUEST_EVERY: Duration = Duration::from_secs(15 * 60);
const REQUEST_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Conexão direta: ping a cada tanto; sem resposta por tanto, caiu.
const LINK_PING: Duration = Duration::from_secs(15);
const LINK_DEAD: Duration = Duration::from_secs(45);
/// Mensagem direta mais velha (ou mais adiantada) que isso é recusada.
const DIRECT_SKEW_MS: i64 = 2 * 60 * 1000;
/// Convite vale por tanto.
const INVITE_TTL_MS: i64 = 30 * 60 * 1000;
const MAX_NAME: usize = 40;
/// Sincronização entre os aparelhos da conta (lista de amigos), a cada tanto e quando muda.
const SELF_EVERY: Duration = Duration::from_secs(2 * 60);
/// Mensagens grandes vão em pedaços (como em `rendezvous`).
const CHUNK: usize = 900;
const MAX_CHUNKS: usize = 8;
/// Pedido de vinculação vale por tanto.
const LINK_TTL: Duration = Duration::from_secs(5 * 60);

const CTX_REQUEST: &[u8] = b"friend request";
const CTX_PRESENCE: &[u8] = b"presence";
const CTX_DIRECT: &[u8] = b"direct";

fn log(msg: impl AsRef<str>) {
    crate::connlog::log(format!("amigos: {}", msg.as_ref()));
}

fn b64(b: &[u8]) -> String {
    B64.encode(b)
}

fn key_of(master: &VerifyingKey) -> String {
    b64(master.as_bytes())
}

fn master_of(s: &str) -> Option<VerifyingKey> {
    VerifyingKey::from_bytes(&B64.decode(s).ok()?.try_into().ok()?).ok()
}

fn box_of(s: &str) -> Option<[u8; 32]> {
    B64.decode(s).ok()?.try_into().ok()
}

fn clean_name(n: &str) -> String {
    let n: String = n.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
    if n.is_empty() { "Sem nome".into() } else { n }
}

/* ================================================================ cifras */

fn seal(key: &[u8; 32], plain: &[u8]) -> Vec<u8> {
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::best());
    let _ = enc.write_all(plain);
    let compressed = enc.finish().unwrap_or_default();
    let mut nonce = [0u8; 12];
    let _ = getrandom::fill(&mut nonce);
    let cipher = ChaCha20Poly1305::new_from_slice(key).expect("chave de 32 bytes");
    let mut out = nonce.to_vec();
    out.extend(cipher.encrypt(&Nonce::from(nonce), compressed.as_slice()).unwrap_or_default());
    out
}

fn open(key: &[u8; 32], sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() <= 12 {
        return None;
    }
    let (nonce, body) = sealed.split_at(12);
    let cipher = ChaCha20Poly1305::new_from_slice(key).ok()?;
    let compressed = cipher.decrypt(&Nonce::from(<[u8; 12]>::try_from(nonce).ok()?), body).ok()?;
    let mut out = Vec::new();
    DeflateDecoder::new(compressed.as_slice()).take(MAX_PLAIN).read_to_end(&mut out).ok()?;
    Some(out)
}

/// Cifra para a chave de caixa de alguém (só ele abre): `efêmera (32) || cifrado`.
fn seal_to(their_box: &[u8; 32], plain: &[u8]) -> Vec<u8> {
    let mut eph = [0u8; 32];
    let _ = getrandom::fill(&mut eph);
    let eph = x25519_dalek::StaticSecret::from(eph);
    let eph_pub = x25519_dalek::PublicKey::from(&eph).to_bytes();
    let dh = eph.diffie_hellman(&x25519_dalek::PublicKey::from(*their_box)).to_bytes();
    let key = blake3::derive_key("antijanja seal v1", &[&dh[..], &eph_pub, their_box].concat());
    [&eph_pub[..], &seal(&key, plain)].concat()
}

fn open_sealed(id: &Identity, sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() <= 32 {
        return None;
    }
    let (eph_pub, body) = sealed.split_at(32);
    let eph_pub: [u8; 32] = eph_pub.try_into().ok()?;
    let dh = id.dh(&eph_pub);
    let key = blake3::derive_key("antijanja seal v1", &[&dh[..], &eph_pub, &id.box_public()].concat());
    open(&key, body)
}

/// Uma mensagem com a assinatura do aparelho que mandou. O corpo vai como JSON (valor),
/// assinado na forma serializada (as chaves saem em ordem: igual dos dois lados).
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Signed {
    b: serde_json::Value,
    s: DeviceSig,
}

impl Signed {
    fn new<T: Serialize>(id: &Identity, ctx: &[u8], body: &T) -> Option<Signed> {
        let b = serde_json::to_value(body).ok()?;
        let bytes = serde_json::to_vec(&b).ok()?;
        Some(Signed { s: id.sign(ctx, &bytes), b })
    }

    fn open<T: for<'de> Deserialize<'de>>(&self, master: &VerifyingKey, revoked: &[String], ctx: &[u8]) -> Option<T> {
        let bytes = serde_json::to_vec(&self.b).ok()?;
        self.s.verify(master, revoked, ctx, &bytes).then(|| serde_json::from_value(self.b.clone()).ok()).flatten()
    }
}

/* ================================================================ chaves derivadas */

fn card_key(master: &VerifyingKey) -> [u8; 32] {
    blake3::derive_key("antijanja card cipher v1", master.as_bytes())
}

fn inbox_writer(master: &VerifyingKey) -> SigningKey {
    SigningKey::from_bytes(&blake3::derive_key("antijanja inbox v1", master.as_bytes()))
}

fn inbox_slot(from: &VerifyingKey, to: &VerifyingKey) -> u8 {
    blake3::hash(&[&from.as_bytes()[..], &to.as_bytes()[..]].concat()).as_bytes()[0] % INBOX_SLOTS
}

fn inbox_salt(slot: u8) -> Vec<u8> {
    format!("rq{slot}").into_bytes()
}

/// Chaves do canal entre duas contas (as duas chegam nas mesmas).
#[derive(Clone)]
struct Pair {
    cipher: [u8; 32],
    secret: [u8; 32],
}

impl Pair {
    fn new(id: &Identity, their_master: &VerifyingKey, their_box: &[u8; 32]) -> Pair {
        let dh = id.dh(their_box);
        let (a, b) = (id.master_pub().to_bytes(), their_master.to_bytes());
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        let secret = blake3::derive_key("antijanja pair v1", &[&dh[..], &lo, &hi].concat());
        Pair { cipher: blake3::derive_key("antijanja pair cipher v1", &secret), secret }
    }

    /// Chave com que `writer` (a conta) publica no canal.
    fn writer(&self, writer: &VerifyingKey) -> SigningKey {
        SigningKey::from_bytes(&blake3::derive_key("antijanja pair writer v1", &[&self.secret[..], writer.as_bytes()].concat()))
    }
}

/* ================================================================ mensagens */

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
struct Card {
    n: String,
    b: String,
    #[serde(default)]
    r: Vec<String>,
    ts: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Request {
    from: String,
    to: String,
    name: String,
    #[serde(rename = "box")]
    box_pub: String,
    ts: i64,
}

/// Uma transmissão minha, contada aos amigos.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Live {
    pub code: String,
    pub title: String,
    pub since: i64,
    /// Também está no mural público.
    #[serde(default)]
    pub public: bool,
    #[serde(default)]
    pub locked: bool,
}

/// O que estou assistindo. O código só vai quando a transmissão é pública.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Watch {
    pub name: String,
    #[serde(default)]
    pub code: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Invite {
    pub code: String,
    pub title: String,
    pub ts: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
struct Presence {
    name: String,
    ts: i64,
    #[serde(default)]
    gone: bool,
    /// Desfez a amizade.
    #[serde(default)]
    removed: bool,
    /// Endereço público e na rede local (socket da fofoca).
    #[serde(default)]
    addr: String,
    #[serde(default)]
    lan: String,
    #[serde(default)]
    live: Option<Live>,
    #[serde(default)]
    watch: Option<Watch>,
    #[serde(default)]
    inv: Option<Invite>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Direct {
    /// "ping", "pong" ou "pres".
    t: String,
    ts: i64,
    #[serde(default)]
    p: Option<Presence>,
}

/* ================================================================ estado */

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct Friend {
    master: String,
    name: String,
    #[serde(rename = "box")]
    box_pub: String,
    since: i64,
    #[serde(default)]
    revoked: Vec<String>,
    /// Última vez visto online (ms).
    #[serde(default)]
    seen: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct Outgoing {
    master: String,
    at: i64,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "box")]
    box_pub: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct Incoming {
    master: String,
    name: String,
    #[serde(rename = "box")]
    box_pub: String,
    at: i64,
    /// Veio da live (assistiu comigo / assisti a dele) ou pelo código.
    #[serde(default)]
    via: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Stored {
    #[serde(default)]
    friends: BTreeMap<String, Friend>,
    #[serde(default)]
    incoming: BTreeMap<String, Incoming>,
    #[serde(default)]
    outgoing: BTreeMap<String, Outgoing>,
    /// Pedidos recusados (não aparecem de novo).
    #[serde(default)]
    ignored: Vec<String>,
    /// Aparelhos desta conta desvinculados (o principal publica no cartão).
    #[serde(default)]
    revoked: Vec<String>,
    /// Aparelhos vinculados por este (no principal).
    #[serde(default)]
    devices: Vec<DeviceCert>,
    /// Amizades desfeitas e pedidos cancelados, com o horário (para a sincronização entre
    /// os aparelhos não trazer de volta).
    #[serde(default)]
    removed: BTreeMap<String, i64>,
}

/// O que os aparelhos da conta sincronizam entre si (canal da própria conta na DHT).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct SelfSync {
    #[serde(default)]
    friends: BTreeMap<String, Friend>,
    #[serde(default)]
    outgoing: BTreeMap<String, Outgoing>,
    #[serde(default)]
    removed: BTreeMap<String, i64>,
    #[serde(default)]
    devices: Vec<DeviceCert>,
    #[serde(default)]
    revoked: Vec<String>,
}

impl SelfSync {
    fn of(st: &Stored) -> SelfSync {
        let strip = |f: &Friend| Friend { seen: 0, ..f.clone() };
        SelfSync {
            friends: st.friends.iter().map(|(k, f)| (k.clone(), strip(f))).collect(),
            outgoing: st.outgoing.clone(),
            removed: st.removed.clone(),
            devices: st.devices.clone(),
            revoked: st.revoked.clone(),
        }
    }

    /// Junta o que veio de outro aparelho no que está gravado aqui. A amizade mais nova que
    /// a remoção vale; a remoção mais nova que a amizade apaga.
    fn merge_into(&self, st: &mut Stored) {
        for (k, t) in &self.removed {
            let e = st.removed.entry(k.clone()).or_insert(*t);
            *e = (*e).max(*t);
        }
        for (k, f) in &self.friends {
            match st.friends.get_mut(k) {
                Some(mine) => {
                    if f.since > mine.since {
                        mine.since = f.since;
                    }
                }
                None => {
                    st.friends.insert(k.clone(), f.clone());
                }
            }
        }
        for (k, o) in &self.outgoing {
            st.outgoing.entry(k.clone()).or_insert_with(|| o.clone());
        }
        let removed = st.removed.clone();
        st.friends.retain(|k, f| removed.get(k).is_none_or(|t| f.since > *t));
        st.outgoing.retain(|k, o| removed.get(k).is_none_or(|t| o.at > *t) && !st.friends.contains_key(k));
        st.incoming.retain(|k, _| !st.friends.contains_key(k));
        for d in &self.devices {
            if !st.devices.iter().any(|x| x.dev == d.dev) {
                st.devices.push(d.clone());
            }
        }
        for r in &self.revoked {
            if !st.revoked.contains(r) {
                st.revoked.push(r.clone());
            }
        }
        let revoked = st.revoked.clone();
        st.devices.retain(|d| !revoked.contains(&d.dev));
    }
}

/// Aparelho novo esperando ser vinculado.
struct LinkWait {
    code: String,
    eph: [u8; 32],
    device: SigningKey,
    figure: [(u8, u8); 3],
    started: Instant,
    error: String,
}

/// Pedido de vinculação achado pelo principal.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkOffer {
    pub code: String,
    pub name: String,
    pub figure: [(u8, u8); 3],
    eph: [u8; 32],
    dev: VerifyingKey,
}

#[derive(Serialize, Deserialize)]
struct LinkAsk {
    eph: String,
    dev: String,
    name: String,
    ts: i64,
}

#[derive(Serialize, Deserialize)]
struct LinkGrant {
    master: String,
    #[serde(rename = "box")]
    box_secret: String,
    cert: DeviceCert,
    name: String,
    sync: SelfSync,
}

#[derive(Default)]
struct Link {
    addr: Option<SocketAddr>,
    last_rx: Option<Instant>,
    last_tx: Option<Instant>,
    /// Tentativas de abrir a conexão (pings para os endereços da presença).
    punch_until: Option<Instant>,
}

#[derive(Default)]
struct State {
    stored: Stored,
    name: String,
    live: Option<Live>,
    watch: Option<Watch>,
    /// Convite pendente para cada amigo.
    invites: HashMap<String, Invite>,
    /// Presença mais nova de cada amigo.
    presence: HashMap<String, Presence>,
    links: HashMap<String, Link>,
    pairs: HashMap<String, Pair>,
    /// Convites recebidos já mostrados (amigo, horário).
    shown_invites: Vec<(String, i64)>,
    /// Publicar a presença para todos já.
    dirty: bool,
    /// Pedidos para mandar já.
    send_now: Vec<String>,
    last_presence_put: Option<Instant>,
    last_poll: HashMap<String, Instant>,
    last_inbox: Option<Instant>,
    last_card: Option<Instant>,
    last_friend_card: HashMap<String, Instant>,
    last_request_put: HashMap<String, Instant>,
    last_self: Option<Instant>,
    /// O que foi sincronizado por último com os outros aparelhos.
    synced: Option<SelfSync>,
    link: Option<LinkWait>,
    stopping: bool,
}

/// Um amigo, como a interface mostra.
#[derive(Clone, Debug, PartialEq)]
pub struct FriendView {
    pub key: String,
    pub name: String,
    pub fingerprint: String,
    pub online: bool,
    pub linked: bool,
    pub seen: i64,
    pub live: Option<Live>,
    pub watch: Option<Watch>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RequestView {
    pub key: String,
    pub name: String,
    pub fingerprint: String,
    pub via: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceView {
    pub dev: String,
    pub name: String,
    pub current: bool,
}

/// Este aparelho esperando ser vinculado: o código e a figura para conferir.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkView {
    pub code: String,
    pub figure: [(u8, u8); 3],
    pub error: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct View {
    pub code: String,
    pub fingerprint: String,
    pub shapes: [(u8, u8); 4],
    pub primary: bool,
    pub device_name: String,
    pub friends: Vec<FriendView>,
    pub incoming: Vec<RequestView>,
    pub outgoing: Vec<RequestView>,
    pub devices: Vec<DeviceView>,
    pub link: Option<LinkView>,
}

pub type ChangeFn = Box<dyn Fn() + Send + Sync>;
/// Convite recebido: (nome do amigo, código, título).
pub type InviteFn = Box<dyn Fn(String, String, String) + Send + Sync>;

pub struct Friends {
    id: RwLock<Arc<Identity>>,
    st: Mutex<State>,
    dht: OnceLock<Dht>,
    on_change: ChangeFn,
    on_invite: InviteFn,
    wake: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    stop: AtomicBool,
    /// Onde gravar (`None` nos testes).
    file: Option<PathBuf>,
}

static GLOBAL: OnceLock<Arc<Friends>> = OnceLock::new();

pub fn get() -> Option<&'static Arc<Friends>> {
    GLOBAL.get()
}

fn store_path() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("APPDATA")?).join("AntiJanja");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("friends.json"))
}

/// Liga as amizades do app (uma vez): conta deste PC, lista gravada, threads.
pub fn start(name: &str, on_change: ChangeFn, on_invite: InviteFn) -> Arc<Friends> {
    let id = identity::load_or_create();
    let f = Friends::new(id, name, store_path(), on_change, on_invite);
    let _ = GLOBAL.set(f.clone());
    let g = f.clone();
    crate::gossip::on_direct(Box::new(move |from, who, data| g.on_direct(from, who, data)));
    f.clone().spawn();
    f
}

/// A DHT ficou pronta (chamado por `rendezvous::init`).
pub fn dht_ready(dht: &Dht) {
    if let Some(f) = GLOBAL.get() {
        let _ = f.dht.set(dht.clone());
        f.wake();
    }
}

impl Friends {
    fn new(id: Identity, name: &str, file: Option<PathBuf>, on_change: ChangeFn, on_invite: InviteFn) -> Arc<Friends> {
        let stored: Stored = file.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let st = State { stored, name: clean_name(name), dirty: true, ..Default::default() };
        Arc::new(Friends {
            id: RwLock::new(Arc::new(id)),
            st: Mutex::new(st),
            dht: OnceLock::new(),
            on_change,
            on_invite,
            wake: Mutex::new(None),
            stop: AtomicBool::new(false),
            file,
        })
    }

    fn id(&self) -> Arc<Identity> {
        self.id.read().unwrap().clone()
    }

    fn save(&self, st: &State) {
        let Some(p) = &self.file else { return };
        if let Ok(json) = serde_json::to_vec_pretty(&st.stored) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(tmp, p);
            }
        }
    }

    fn wake(&self) {
        if let Some(w) = self.wake.lock().unwrap().as_ref() {
            let _ = w.send(());
        }
    }

    fn changed(&self) {
        (self.on_change)();
    }

    /* ------------------------------------------------------------ para a interface */

    pub fn view(&self) -> View {
        let id = self.id();
        let (shapes, fp) = id.fingerprint();
        let st = self.st.lock().unwrap();
        let now = now_ms();
        let mut friends: Vec<FriendView> = st
            .stored
            .friends
            .values()
            .map(|f| {
                let p = st.presence.get(&f.master);
                let linked = st.links.get(&f.master).is_some_and(|l| l.last_rx.is_some_and(|t| t.elapsed() < LINK_DEAD));
                let fresh = p.is_some_and(|p| !p.gone && now - p.ts < PRESENCE_FRESH_MS);
                let online = linked || fresh;
                FriendView {
                    key: f.master.clone(),
                    name: f.name.clone(),
                    fingerprint: master_of(&f.master).map(|m| identity::fingerprint(m.as_bytes()).1).unwrap_or_default(),
                    online,
                    linked,
                    seen: f.seen,
                    live: p.filter(|_| online).and_then(|p| p.live.clone()),
                    watch: p.filter(|_| online).and_then(|p| p.watch.clone()),
                }
            })
            .collect();
        // Ao vivo, depois online, depois visto há menos tempo.
        friends.sort_by(|a, b| (b.live.is_some(), b.online, b.seen).cmp(&(a.live.is_some(), a.online, a.seen)).then(a.name.cmp(&b.name)));
        let req = |key: &str, name: &str, via: &str| RequestView {
            key: key.to_string(),
            name: name.to_string(),
            fingerprint: master_of(key).map(|m| identity::fingerprint(m.as_bytes()).1).unwrap_or_default(),
            via: via.to_string(),
        };
        View {
            code: id.friend_code(),
            fingerprint: fp,
            shapes,
            primary: id.is_primary(),
            device_name: id.cert().name.clone(),
            friends,
            incoming: st.stored.incoming.values().map(|r| req(&r.master, &r.name, &r.via)).collect(),
            outgoing: st.stored.outgoing.values().map(|r| req(&r.master, &r.name, "")).collect(),
            devices: std::iter::once(DeviceView { dev: id.cert().dev.clone(), name: id.cert().name.clone(), current: true })
                .chain(st.stored.devices.iter().filter(|d| d.dev != id.cert().dev).map(|d| DeviceView { dev: d.dev.clone(), name: d.name.clone(), current: false }))
                .collect(),
            link: st.link.as_ref().map(|l| LinkView { code: l.code.clone(), figure: l.figure, error: l.error.clone() }),
        }
    }

    pub fn my_code(&self) -> String {
        self.id().friend_code()
    }

    /// O que sou de quem tem este código: "me", "friend", "sent" (pedido enviado),
    /// "incoming" (ele pediu) ou "" (nada).
    pub fn relation(&self, code: &str) -> &'static str {
        let Ok(m) = identity::parse_friend_code(code) else { return "" };
        if m == self.id().master_pub() {
            return "me";
        }
        let k = key_of(&m);
        let st = self.st.lock().unwrap();
        if st.stored.friends.contains_key(&k) {
            "friend"
        } else if st.stored.outgoing.contains_key(&k) {
            "sent"
        } else if st.stored.incoming.contains_key(&k) {
            "incoming"
        } else {
            ""
        }
    }

    pub fn phrase(&self) -> Option<String> {
        self.id().phrase()
    }

    pub fn set_name(&self, name: &str) {
        let mut st = self.st.lock().unwrap();
        let n = clean_name(name);
        if st.name != n {
            st.name = n;
            st.dirty = true;
            st.last_card = None;
            drop(st);
            self.wake();
        }
    }

    pub fn set_live(&self, live: Option<Live>) {
        let mut st = self.st.lock().unwrap();
        if st.live != live {
            st.live = live;
            st.dirty = true;
            drop(st);
            self.push_presence_direct();
            self.wake();
        }
    }

    pub fn set_watch(&self, watch: Option<Watch>) {
        let mut st = self.st.lock().unwrap();
        if st.watch != watch {
            st.watch = watch;
            st.dirty = true;
            drop(st);
            self.push_presence_direct();
            self.wake();
        }
    }

    /// Manda pedido de amizade para um código `AJF.…`.
    pub fn add(&self, code: &str) -> Result<(), String> {
        let master = identity::parse_friend_code(code)?;
        let id = self.id();
        if master == id.master_pub() {
            return Err("Esse é o seu próprio código.".into());
        }
        let key = key_of(&master);
        let mut st = self.st.lock().unwrap();
        if st.stored.friends.contains_key(&key) {
            return Err("Vocês já são amigos.".into());
        }
        // Ele já me pediu: aceitar.
        if st.stored.incoming.contains_key(&key) {
            drop(st);
            return self.accept(&key);
        }
        st.stored.ignored.retain(|k| *k != key);
        st.stored.outgoing.entry(key.clone()).or_insert(Outgoing { master: key.clone(), at: now_ms(), ..Default::default() });
        st.send_now.push(key);
        self.save(&st);
        drop(st);
        self.changed();
        self.wake();
        Ok(())
    }

    pub fn accept(&self, key: &str) -> Result<(), String> {
        let mut st = self.st.lock().unwrap();
        let r = st.stored.incoming.remove(key).ok_or("Pedido não encontrado.")?;
        st.stored.outgoing.remove(key);
        st.stored.friends.insert(key.to_string(), Friend { master: r.master, name: r.name, box_pub: r.box_pub, since: now_ms(), ..Default::default() });
        // A presença no canal do par é o "aceito".
        st.dirty = true;
        self.save(&st);
        drop(st);
        log("pedido aceito");
        self.changed();
        self.wake();
        Ok(())
    }

    pub fn decline(&self, key: &str) {
        let mut st = self.st.lock().unwrap();
        if st.stored.incoming.remove(key).is_some() {
            st.stored.ignored.push(key.to_string());
        }
        if st.stored.outgoing.remove(key).is_some() {
            st.stored.removed.insert(key.to_string(), now_ms());
        }
        self.save(&st);
        drop(st);
        self.changed();
    }

    /// Desfaz a amizade (o outro lado fica sabendo pela presença `removed`).
    pub fn remove(self: &Arc<Self>, key: &str) {
        let removed = {
            let mut st = self.st.lock().unwrap();
            let f = st.stored.friends.remove(key);
            st.stored.removed.insert(key.to_string(), now_ms());
            st.presence.remove(key);
            st.links.remove(key);
            self.save(&st);
            f
        };
        if let Some(f) = removed {
            let me = self.clone();
            thread::spawn(move || {
                if let Some(dht) = me.dht.get() {
                    let p = Presence { name: me.st.lock().unwrap().name.clone(), ts: now_ms(), removed: true, ..Default::default() };
                    let _ = me.put_presence(dht, &f, &p);
                }
            });
        }
        self.changed();
    }

    /// Convida um amigo para a minha live.
    pub fn invite(&self, key: &str) -> Result<(), String> {
        let mut st = self.st.lock().unwrap();
        let live = st.live.clone().ok_or("Você não está transmitindo.")?;
        st.invites.insert(key.to_string(), Invite { code: live.code, title: live.title, ts: now_ms() });
        st.dirty = true;
        drop(st);
        self.push_presence_direct();
        self.wake();
        Ok(())
    }

    /// Troca a conta deste PC (restaurada pela frase). A lista de amigos da conta anterior
    /// fica guardada à parte; se este PC já usou a nova, a lista dela volta.
    pub fn switch_account(&self, id: Identity) -> Result<(), String> {
        let old = self.id();
        if old.master_pub() == id.master_pub() {
            return Err("Essa já é a conta deste PC.".into());
        }
        let stored = self.aside(&id.master_pub()).and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok());
        self.adopt(id, stored)?;
        log("conta trocada pela frase de recuperação");
        Ok(())
    }

    /// Onde fica guardada a lista de amigos de uma conta que este PC deixou de usar.
    fn aside(&self, m: &VerifyingKey) -> Option<PathBuf> {
        self.file.as_ref().map(|p| p.with_file_name(format!("friends-{}.json", &key_of(m)[..12])))
    }

    /// Passa a usar `id` neste PC, com esta lista de amigos (a da conta anterior fica
    /// guardada à parte).
    fn adopt(&self, id: Identity, stored: Option<Stored>) -> Result<(), String> {
        if self.file.is_some() {
            identity::save(&id)?;
        }
        let mut st = self.st.lock().unwrap();
        if let (Some(p), Ok(json)) = (self.aside(&self.id().master_pub()), serde_json::to_vec_pretty(&st.stored)) {
            let _ = std::fs::write(p, json);
        }
        let (name, live, watch) = (st.name.clone(), st.live.clone(), st.watch.clone());
        *st = State { name, live, watch, dirty: true, ..Default::default() };
        st.stored = stored.unwrap_or_default();
        *self.id.write().unwrap() = Arc::new(id);
        self.save(&st);
        drop(st);
        self.changed();
        self.wake();
        Ok(())
    }

    /// Avisa os amigos que estou saindo (presença `gone`), esperando a DHT por até `max`.
    pub fn shutdown_within(self: &Arc<Self>, max: Duration) {
        let friends: Vec<Friend> = {
            let mut st = self.st.lock().unwrap();
            st.stopping = true;
            st.stored.friends.values().cloned().collect()
        };
        self.push_presence_direct();
        self.stop.store(true, Ordering::SeqCst);
        self.wake();
        let Some(dht) = self.dht.get().cloned() else { return };
        let me = self.clone();
        let h = thread::spawn(move || {
            let p = me.my_presence(None, true);
            par(friends.into_iter().map(|f| {
                let (me, dht, p) = (me.clone(), dht.clone(), p.clone());
                move || {
                    let _ = me.put_presence(&dht, &f, &p);
                }
            }));
        });
        let until = Instant::now() + max;
        while !h.is_finished() && Instant::now() < until {
            thread::sleep(Duration::from_millis(50));
        }
    }

    /* ------------------------------------------------------------ threads */

    fn spawn(self: Arc<Self>) {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        *self.wake.lock().unwrap() = Some(tx);
        let me = self.clone();
        let _ = thread::Builder::new().name("aj-friends-dht".into()).spawn(move || me.dht_loop(rx));
        let me = self;
        let _ = thread::Builder::new().name("aj-friends-link".into()).spawn(move || me.link_loop());
    }

    fn dht_loop(self: Arc<Self>, wake: std::sync::mpsc::Receiver<()>) {
        while !self.stop.load(Ordering::SeqCst) {
            if let Some(dht) = self.dht.get().cloned() {
                self.dht_round(&dht);
            }
            let _ = wake.recv_timeout(Duration::from_secs(10));
        }
    }

    /// Uma volta: cartão, pedidos, presença (minha e dos amigos), caixa de entrada.
    fn dht_round(self: &Arc<Self>, dht: &Dht) {
        let due = |t: Option<&Instant>, every: Duration| t.is_none_or(|t| t.elapsed() >= every);
        // Cartão da conta.
        if due(self.st.lock().unwrap().last_card.as_ref(), CARD_EVERY) {
            self.st.lock().unwrap().last_card = Some(Instant::now());
            self.publish_card(dht);
        }
        // Pedidos enviados: mandar (e republicar) e ver se foram aceitos.
        let outgoing: Vec<Outgoing> = self.st.lock().unwrap().stored.outgoing.values().cloned().collect();
        for o in outgoing {
            let (now, last) = {
                let mut st = self.st.lock().unwrap();
                let now = st.send_now.contains(&o.master);
                st.send_now.retain(|k| *k != o.master);
                (now, st.last_request_put.get(&o.master).copied())
            };
            if now_ms() - o.at > REQUEST_TTL_MS {
                let mut st = self.st.lock().unwrap();
                st.stored.outgoing.remove(&o.master);
                self.save(&st);
                continue;
            }
            if now || due(last.as_ref(), REQUEST_EVERY) {
                self.st.lock().unwrap().last_request_put.insert(o.master.clone(), Instant::now());
                self.st.lock().unwrap().last_poll.insert(o.master.clone(), Instant::now());
                self.send_request(dht, &o);
            } else if due(self.st.lock().unwrap().last_poll.get(&o.master), POLL_FEW) {
                self.st.lock().unwrap().last_poll.insert(o.master.clone(), Instant::now());
                self.check_accepted(dht, &o);
            }
        }
        // Minha presença, para cada amigo.
        let (dirty, last) = {
            let st = self.st.lock().unwrap();
            (st.dirty, st.last_presence_put)
        };
        if dirty || due(last.as_ref(), PRESENCE_EVERY) {
            {
                let mut st = self.st.lock().unwrap();
                st.dirty = false;
                st.last_presence_put = Some(Instant::now());
            }
            let friends: Vec<Friend> = self.st.lock().unwrap().stored.friends.values().cloned().collect();
            let me = self.clone();
            par(friends.into_iter().map(|f| {
                let (me, dht) = (me.clone(), dht.clone());
                move || {
                    let p = me.my_presence(Some(&f.master), false);
                    if let Err(e) = me.put_presence(&dht, &f, &p) {
                        log(format!("presença não publicada: {e}"));
                    }
                }
            }));
        }
        // Presença dos amigos.
        let to_poll: Vec<Friend> = {
            let st = self.st.lock().unwrap();
            st.stored
                .friends
                .values()
                .filter(|f| {
                    let linked = st.links.get(&f.master).is_some_and(|l| l.last_rx.is_some_and(|t| t.elapsed() < LINK_DEAD));
                    due(st.last_poll.get(&f.master), if linked { POLL_LINKED } else { POLL_FEW })
                })
                .cloned()
                .collect()
        };
        if !to_poll.is_empty() {
            let me = self.clone();
            let got = par(to_poll.into_iter().map(|f| {
                let (me, dht) = (me.clone(), dht.clone());
                move || {
                    me.st.lock().unwrap().last_poll.insert(f.master.clone(), Instant::now());
                    let p = me.fetch_presence(&dht, &f);
                    (f, p)
                }
            }));
            for (f, p) in got {
                if let Some(p) = p {
                    self.got_presence(&f.master, p);
                }
            }
        }
        // Cartão dos amigos (nome e aparelhos desvinculados).
        let card_due: Vec<String> = {
            let st = self.st.lock().unwrap();
            st.stored.friends.keys().filter(|k| due(st.last_friend_card.get(*k), FRIEND_CARD_EVERY)).cloned().collect()
        };
        for k in card_due {
            self.st.lock().unwrap().last_friend_card.insert(k.clone(), Instant::now());
            if let Some(card) = master_of(&k).and_then(|m| fetch_card(dht, &m)) {
                let mut st = self.st.lock().unwrap();
                if let Some(f) = st.stored.friends.get_mut(&k) {
                    f.revoked = card.r.clone();
                }
                self.save(&st);
            }
        }
        // Caixa de entrada.
        if due(self.st.lock().unwrap().last_inbox.as_ref(), INBOX_EVERY) {
            self.st.lock().unwrap().last_inbox = Some(Instant::now());
            self.read_inbox(dht);
        }
        // Outros aparelhos da conta.
        let (changed, last) = {
            let st = self.st.lock().unwrap();
            (st.synced.as_ref() != Some(&SelfSync::of(&st.stored)), st.last_self)
        };
        if changed || due(last.as_ref(), SELF_EVERY) {
            self.st.lock().unwrap().last_self = Some(Instant::now());
            self.self_sync(dht);
        }
    }

    /// Canal da própria conta: cifra e chave de escrita saem da chave de caixa (todos os
    /// aparelhos da conta têm).
    fn self_keys(&self) -> ([u8; 32], SigningKey) {
        let b = self.id().box_secret_bytes();
        (blake3::derive_key("antijanja self cipher v1", &b), SigningKey::from_bytes(&blake3::derive_key("antijanja self writer v1", &b)))
    }

    /// Lê o que os outros aparelhos publicaram, junta com o daqui e publica o resultado
    /// se mudou alguma coisa.
    fn self_sync(&self, dht: &Dht) {
        let (cipher, writer) = self.self_keys();
        let remote: Option<SelfSync> = get_chunked(dht, writer.verifying_key().as_bytes(), "s").and_then(|v| open(&cipher, &v)).and_then(|j| serde_json::from_slice(&j).ok());
        let (merged, publish, grew) = {
            let mut st = self.st.lock().unwrap();
            let before = SelfSync::of(&st.stored);
            if let Some(r) = &remote {
                r.merge_into(&mut st.stored);
            }
            let merged = SelfSync::of(&st.stored);
            let grew = merged != before;
            if grew {
                st.dirty = true;
                st.last_card = None;
                self.save(&st);
            }
            (merged.clone(), remote.as_ref() != Some(&merged), grew)
        };
        if grew {
            log("lista de amigos atualizada por outro aparelho");
            self.changed();
        }
        if publish {
            let Ok(json) = serde_json::to_vec(&merged) else { return };
            match put_chunked(dht, &writer, "s", &seal(&cipher, &json)) {
                Ok(()) => log("lista de amigos sincronizada com os outros aparelhos"),
                Err(e) => return log(format!("sincronização não publicada: {e}")),
            }
        }
        self.st.lock().unwrap().synced = Some(merged);
    }

    /* ------------------------------------------------------------ vincular aparelhos */

    /// Aparelho novo: pede para ser vinculado a uma conta que já existe. Mostra o código
    /// (6 dígitos) e a figura; fica esperando o principal confirmar.
    pub fn link_request(self: &Arc<Self>, device_name: &str) {
        let mut d = [0u8; 4];
        let _ = getrandom::fill(&mut d);
        let code = format!("{:06}", u32::from_le_bytes(d) % 1_000_000);
        let mut eph = [0u8; 32];
        let _ = getrandom::fill(&mut eph);
        let mut dev = [0u8; 32];
        let _ = getrandom::fill(&mut dev);
        let device = SigningKey::from_bytes(&dev);
        let eph_pub = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(eph)).to_bytes();
        let figure = link_figure(&eph_pub, &device.verifying_key());
        let ask = LinkAsk { eph: b64(&eph_pub), dev: key_of(&device.verifying_key()), name: device_name.chars().take(32).collect(), ts: now_ms() };
        self.st.lock().unwrap().link = Some(LinkWait { code: code.clone(), eph, device, figure, started: Instant::now(), error: String::new() });
        self.changed();
        let me = self.clone();
        thread::spawn(move || me.link_wait(code, ask));
    }

    pub fn link_cancel(&self) {
        self.st.lock().unwrap().link = None;
        self.changed();
    }

    fn link_wait(self: Arc<Self>, code: String, ask: LinkAsk) {
        let still = |me: &Friends| me.st.lock().unwrap().link.as_ref().is_some_and(|l| l.code == code && l.started.elapsed() < LINK_TTL);
        let fail = |me: &Friends, msg: &str| {
            if let Some(l) = me.st.lock().unwrap().link.as_mut().filter(|l| l.code == code) {
                l.error = msg.to_string();
            }
            me.changed();
        };
        let dht = loop {
            if let Some(d) = self.dht.get() {
                break d.clone();
            }
            if !still(&self) {
                return;
            }
            thread::sleep(Duration::from_millis(500));
        };
        let l = link_signer(&code);
        let Ok(json) = serde_json::to_vec(&ask) else { return };
        let mut last_put: Option<Instant> = None;
        while still(&self) {
            // O pedido fica publicado (republica de vez em quando, para os nós certos).
            if last_put.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
                last_put = Some(Instant::now());
                if let Err(e) = dht.put_mutable(MutableItem::new(l.clone(), &json, now_ms(), Some(b"q")), None) {
                    log(format!("vincular: pedido não publicado: {e}"));
                }
            }
            if let Some(v) = get_chunked(&dht, l.verifying_key().as_bytes(), "r") {
                let (eph, device) = match self.st.lock().unwrap().link.as_ref() {
                    Some(w) => (w.eph, w.device.clone()),
                    None => return,
                };
                match open_grant(&v, &eph, &code).and_then(|g| {
                    let master = master_of(&g.master)?;
                    let id = Identity::linked(master, box_of(&g.box_secret)?, device, g.cert.clone()).ok()?;
                    Some((id, g))
                }) {
                    Some((id, g)) => {
                        let mut stored = Stored::default();
                        g.sync.merge_into(&mut stored);
                        if let Err(e) = self.adopt(id, Some(stored)) {
                            return fail(&self, &e);
                        }
                        log("este aparelho foi vinculado");
                        return;
                    }
                    None => return fail(&self, "A resposta do outro aparelho veio estragada. Gere outro código."),
                }
            }
            thread::sleep(Duration::from_secs(3));
        }
        if self.st.lock().unwrap().link.as_ref().is_some_and(|l| l.code == code) {
            fail(&self, "O código expirou. Gere outro.");
        }
    }

    /// Principal: procura o pedido do aparelho novo pelo código (demora alguns segundos).
    pub fn link_lookup(&self, code: &str) -> Result<LinkOffer, String> {
        if !self.id().is_primary() {
            return Err("Só o aparelho principal (onde a conta foi criada) vincula aparelhos.".into());
        }
        let code: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
        if code.len() != 6 {
            return Err("O código tem 6 números.".into());
        }
        let dht = self.dht.get().ok_or("Ainda entrando na rede P2P.")?.clone();
        let item = dht.get_mutable_most_recent(link_signer(&code).verifying_key().as_bytes(), Some(b"q")).ok_or("Nenhum aparelho com esse código. Confira os números.")?;
        let ask: LinkAsk = serde_json::from_slice(item.value()).map_err(|_| "Pedido inválido.")?;
        if now_ms() - ask.ts > LINK_TTL.as_millis() as i64 + 60_000 {
            return Err("Esse código expirou. Gere outro no aparelho novo.".into());
        }
        let eph = box_of(&ask.eph).ok_or("Pedido inválido.")?;
        let dev = master_of(&ask.dev).ok_or("Pedido inválido.")?;
        Ok(LinkOffer { code, name: clean_name(&ask.name), figure: link_figure(&eph, &dev), eph, dev })
    }

    /// Principal: a figura bateu; assina o certificado do aparelho novo e manda a conta
    /// (chave pública, chave de caixa, certificado e a lista de amigos), cifrada para ele.
    pub fn link_confirm(&self, offer: &LinkOffer, device_name: &str) -> Result<(), String> {
        let id = self.id();
        let name = if device_name.trim().is_empty() { offer.name.clone() } else { device_name.trim().chars().take(32).collect() };
        let cert = id.issue_cert(&offer.dev, &name).ok_or("Só o aparelho principal vincula aparelhos.")?;
        let dht = self.dht.get().ok_or("Ainda entrando na rede P2P.")?.clone();
        let sync = {
            let mut st = self.st.lock().unwrap();
            st.stored.devices.retain(|d| d.dev != cert.dev);
            st.stored.devices.push(cert.clone());
            self.save(&st);
            SelfSync::of(&st.stored)
        };
        let grant = LinkGrant { master: key_of(&id.master_pub()), box_secret: b64(&id.box_secret_bytes()), cert, name: self.st.lock().unwrap().name.clone(), sync };
        let json = serde_json::to_vec(&grant).map_err(|e| e.to_string())?;
        let mut eph = [0u8; 32];
        getrandom::fill(&mut eph).map_err(|e| e.to_string())?;
        let eph = x25519_dalek::StaticSecret::from(eph);
        let eph_pub = x25519_dalek::PublicKey::from(&eph).to_bytes();
        let dh = eph.diffie_hellman(&x25519_dalek::PublicKey::from(offer.eph)).to_bytes();
        let key = blake3::derive_key("antijanja link seal v1", &[&dh[..], offer.code.as_bytes()].concat());
        let value = [&eph_pub[..], &seal(&key, &json)].concat();
        put_chunked(&dht, &link_signer(&offer.code), "r", &value)?;
        log("aparelho vinculado");
        self.changed();
        self.wake();
        Ok(())
    }

    /// Principal: desvincula um aparelho (entra no cartão; os amigos passam a recusar).
    pub fn revoke(&self, dev: &str) -> Result<(), String> {
        let id = self.id();
        if !id.is_primary() {
            return Err("Só o aparelho principal desvincula aparelhos.".into());
        }
        if id.cert().dev == dev {
            return Err("Esse é este aparelho.".into());
        }
        let mut st = self.st.lock().unwrap();
        st.stored.devices.retain(|d| d.dev != dev);
        if !st.stored.revoked.iter().any(|r| r == dev) {
            st.stored.revoked.push(dev.to_string());
        }
        st.last_card = None;
        self.save(&st);
        drop(st);
        self.changed();
        self.wake();
        Ok(())
    }

    fn publish_card(&self, dht: &Dht) {
        let id = self.id();
        let master = id.master_pub();
        match id.master() {
            Some(sk) => {
                let st = self.st.lock().unwrap();
                let card = Card { n: st.name.clone(), b: b64(&id.box_public()), r: st.stored.revoked.clone(), ts: now_ms() };
                drop(st);
                let Ok(json) = serde_json::to_vec(&card) else { return };
                let value = seal(&card_key(&master), &json);
                if value.len() > MAX_VALUE {
                    return log("cartão grande demais");
                }
                match dht.put_mutable(MutableItem::new(sk.clone(), &value, now_ms(), Some(b"card")), None) {
                    Ok(_) => log("cartão publicado"),
                    Err(e) => log(format!("cartão não publicado: {e}")),
                }
            }
            // Aparelho vinculado: republica o cartão que o principal assinou.
            None => {
                if let Some(item) = dht.get_mutable_most_recent(master.as_bytes(), Some(b"card")) {
                    let _ = dht.put_mutable(item, None);
                }
            }
        }
    }

    fn send_request(&self, dht: &Dht, o: &Outgoing) {
        let Some(their) = master_of(&o.master) else { return };
        // Chave de caixa dele: do cartão.
        let card = if o.box_pub.is_empty() { fetch_card(dht, &their) } else { None };
        let (name, box_pub) = match &card {
            Some(c) => (clean_name(&c.n), c.b.clone()),
            None => (o.name.clone(), o.box_pub.clone()),
        };
        let Some(their_box) = box_of(&box_pub) else {
            return log("pedido: cartão do amigo não encontrado (ele precisa abrir o AntiJanja)");
        };
        {
            let mut st = self.st.lock().unwrap();
            if let Some(x) = st.stored.outgoing.get_mut(&o.master) {
                x.name = name;
                x.box_pub = box_pub;
            }
            self.save(&st);
        }
        self.changed();
        let id = self.id();
        let req = Request { from: key_of(&id.master_pub()), to: o.master.clone(), name: self.st.lock().unwrap().name.clone(), box_pub: b64(&id.box_public()), ts: now_ms() };
        let Some(signed) = Signed::new(&id, CTX_REQUEST, &req) else { return };
        let Ok(json) = serde_json::to_vec(&signed) else { return };
        let value = seal_to(&their_box, &json);
        if value.len() > MAX_VALUE {
            return log(format!("pedido grande demais ({} bytes)", value.len()));
        }
        let slot = inbox_slot(&id.master_pub(), &their);
        match dht.put_mutable(MutableItem::new(inbox_writer(&their), &value, now_ms(), Some(&inbox_salt(slot))), None) {
            Ok(_) => log(format!("pedido enviado (vaga {slot})")),
            Err(e) => log(format!("pedido não enviado: {e}")),
        }
        // (A trava do estado não pode ficar presa na chamada: `check_accepted` trava de novo.)
        let current = self.st.lock().unwrap().stored.outgoing.get(&o.master).cloned();
        self.check_accepted(dht, &current.unwrap_or_else(|| o.clone()));
    }

    /// Pedido enviado: ele já publicou presença no nosso canal? Então aceitou.
    fn check_accepted(&self, dht: &Dht, o: &Outgoing) {
        if o.box_pub.is_empty() {
            return;
        }
        let f = Friend { master: o.master.clone(), name: o.name.clone(), box_pub: o.box_pub.clone(), since: now_ms(), ..Default::default() };
        let Some(p) = self.fetch_presence(dht, &f) else { return };
        if p.removed {
            return;
        }
        {
            let mut st = self.st.lock().unwrap();
            if st.stored.outgoing.remove(&o.master).is_none() {
                return;
            }
            st.stored.friends.insert(o.master.clone(), f);
            st.dirty = true;
            self.save(&st);
        }
        log("pedido aceito pelo outro lado");
        self.got_presence(&o.master, p);
        self.changed();
    }

    fn read_inbox(&self, dht: &Dht) {
        let id = self.id();
        let pk = inbox_writer(&id.master_pub()).verifying_key().to_bytes();
        let values = par((0..INBOX_SLOTS).map(|slot| {
            let dht = dht.clone();
            move || dht.get_mutable_most_recent(&pk, Some(&inbox_salt(slot))).map(|it| it.value().to_vec())
        }));
        let mut new = false;
        for v in values.into_iter().flatten() {
            let Some(req) = open_request(&id, &v) else { continue };
            let mut st = self.st.lock().unwrap();
            if st.stored.friends.contains_key(&req.from) || st.stored.ignored.contains(&req.from) || st.stored.incoming.contains_key(&req.from) {
                continue;
            }
            // Os dois se adicionaram: vira amizade direto.
            if st.stored.outgoing.remove(&req.from).is_some() {
                st.stored.friends.insert(req.from.clone(), Friend { master: req.from.clone(), name: clean_name(&req.name), box_pub: req.box_pub.clone(), since: now_ms(), ..Default::default() });
                st.dirty = true;
            } else {
                st.stored.incoming.insert(req.from.clone(), Incoming { master: req.from.clone(), name: clean_name(&req.name), box_pub: req.box_pub.clone(), at: now_ms(), via: "code".into() });
            }
            self.save(&st);
            new = true;
        }
        if new {
            log("pedido de amizade recebido");
            self.changed();
        }
    }

    fn pair(&self, f: &Friend) -> Option<Pair> {
        if let Some(p) = self.st.lock().unwrap().pairs.get(&f.master) {
            return Some(p.clone());
        }
        let (m, b) = (master_of(&f.master)?, box_of(&f.box_pub)?);
        let p = Pair::new(&self.id(), &m, &b);
        self.st.lock().unwrap().pairs.insert(f.master.clone(), p.clone());
        Some(p)
    }

    /// Minha presença (para um amigo: com o convite dele, se houver).
    fn my_presence(&self, for_friend: Option<&str>, gone: bool) -> Presence {
        let st = self.st.lock().unwrap();
        let (addr, lan) = if gone { (String::new(), String::new()) } else { my_addrs() };
        Presence {
            name: st.name.clone(),
            ts: now_ms(),
            gone,
            removed: false,
            addr,
            lan,
            live: if gone { None } else { st.live.clone() },
            watch: if gone { None } else { st.watch.clone() },
            // Convite só enquanto a live dele está no ar.
            inv: for_friend
                .and_then(|k| st.invites.get(k))
                .filter(|i| now_ms() - i.ts < INVITE_TTL_MS && !gone && st.live.as_ref().is_some_and(|l| l.code == i.code))
                .cloned(),
        }
    }

    fn put_presence(&self, dht: &Dht, f: &Friend, p: &Presence) -> Result<(), String> {
        let id = self.id();
        let pair = self.pair(f).ok_or("amigo sem chave")?;
        let signed = Signed::new(&id, CTX_PRESENCE, p).ok_or("assinatura")?;
        let json = serde_json::to_vec(&signed).map_err(|e| e.to_string())?;
        let mut value = seal(&pair.cipher, &json);
        if value.len() > MAX_VALUE {
            // Sem título e sem o que estou assistindo, para caber.
            let mut small = p.clone();
            if let Some(l) = small.live.as_mut() {
                l.title.clear();
            }
            small.watch = None;
            let signed = Signed::new(&id, CTX_PRESENCE, &small).ok_or("assinatura")?;
            value = seal(&pair.cipher, &serde_json::to_vec(&signed).map_err(|e| e.to_string())?);
            if value.len() > MAX_VALUE {
                return Err(format!("presença grande demais ({} bytes)", value.len()));
            }
        }
        dht.put_mutable(MutableItem::new(pair.writer(&id.master_pub()), &value, now_ms(), Some(b"p")), None).map(|_| ()).map_err(|e| e.to_string())
    }

    fn fetch_presence(&self, dht: &Dht, f: &Friend) -> Option<Presence> {
        let pair = self.pair(f)?;
        let their = master_of(&f.master)?;
        let item = dht.get_mutable_most_recent(pair.writer(&their).verifying_key().as_bytes(), Some(b"p"))?;
        let plain = open(&pair.cipher, item.value())?;
        let signed: Signed = serde_json::from_slice(&plain).ok()?;
        signed.open(&their, &f.revoked, CTX_PRESENCE)
    }

    fn got_presence(&self, key: &str, p: Presence) {
        let mut notify_invite: Option<(String, Invite)> = None;
        let mut punch: Vec<SocketAddr> = Vec::new();
        {
            let mut st = self.st.lock().unwrap();
            if st.presence.get(key).is_some_and(|old| old.ts >= p.ts) {
                return;
            }
            if p.removed {
                st.stored.friends.remove(key);
                st.presence.remove(key);
                st.links.remove(key);
                self.save(&st);
                drop(st);
                log("um amigo desfez a amizade");
                return self.changed();
            }
            let fresh = !p.gone && now_ms() - p.ts < PRESENCE_FRESH_MS;
            let name = clean_name(&p.name);
            if let Some(f) = st.stored.friends.get_mut(key) {
                if fresh {
                    f.seen = p.ts;
                }
                if f.name != name {
                    f.name = name.clone();
                }
            }
            if let Some(inv) = p.inv.clone().filter(|i| now_ms() - i.ts < INVITE_TTL_MS && !i.code.is_empty()) {
                if !st.shown_invites.contains(&(key.to_string(), inv.ts)) {
                    st.shown_invites.push((key.to_string(), inv.ts));
                    notify_invite = Some((name.clone(), inv));
                }
            }
            if fresh {
                let linked = st.links.get(key).is_some_and(|l| l.last_rx.is_some_and(|t| t.elapsed() < LINK_DEAD));
                if !linked {
                    punch = [&p.addr, &p.lan].iter().filter_map(|a| a.parse().ok()).collect();
                    let l = st.links.entry(key.to_string()).or_default();
                    l.punch_until = Some(Instant::now() + Duration::from_secs(60));
                }
            }
            st.presence.insert(key.to_string(), p);
            self.save(&st);
        }
        for a in punch {
            self.send_direct_to(key, a, "ping", None);
        }
        if let Some((name, inv)) = notify_invite {
            (self.on_invite)(name, inv.code, inv.title);
        }
        self.changed();
    }

    /* ------------------------------------------------------------ conexão direta */

    fn send_direct_to(&self, key: &str, to: SocketAddr, t: &str, p: Option<Presence>) {
        let Some(f) = self.st.lock().unwrap().stored.friends.get(key).cloned() else { return };
        let Some(pair) = self.pair(&f) else { return };
        let id = self.id();
        let Some(signed) = Signed::new(&id, CTX_DIRECT, &Direct { t: t.into(), ts: now_ms(), p }) else { return };
        let Ok(json) = serde_json::to_vec(&signed) else { return };
        let d = b64(&seal(&pair.cipher, &json));
        if crate::gossip::send_direct(to, &key_of(&id.master_pub()), &d) {
            if let Some(l) = self.st.lock().unwrap().links.get_mut(key) {
                l.last_tx = Some(Instant::now());
            }
        }
    }

    /// Manda minha presença já para os amigos com conexão direta.
    fn push_presence_direct(&self) {
        let targets: Vec<(String, SocketAddr)> = {
            let st = self.st.lock().unwrap();
            st.links.iter().filter(|(_, l)| l.last_rx.is_some_and(|t| t.elapsed() < LINK_DEAD)).filter_map(|(k, l)| Some((k.clone(), l.addr?))).collect()
        };
        let gone = self.st.lock().unwrap().stopping;
        for (k, a) in targets {
            let p = self.my_presence(Some(&k), gone);
            self.send_direct_to(&k, a, "pres", Some(p));
        }
    }

    /// Mensagem direta chegando pelo socket da fofoca.
    fn on_direct(&self, from: SocketAddr, who: &str, data: &str) {
        let Some(f) = self.st.lock().unwrap().stored.friends.get(who).cloned() else { return };
        let (Some(pair), Some(master)) = (self.pair(&f), master_of(who)) else { return };
        let Some(plain) = B64.decode(data).ok().and_then(|b| open(&pair.cipher, &b)) else { return };
        let Some(signed) = serde_json::from_slice::<Signed>(&plain).ok() else { return };
        let Some(msg) = signed.open::<Direct>(&master, &f.revoked, CTX_DIRECT) else { return };
        if (now_ms() - msg.ts).abs() > DIRECT_SKEW_MS {
            return;
        }
        let first = {
            let mut st = self.st.lock().unwrap();
            let l = st.links.entry(who.to_string()).or_default();
            let first = l.last_rx.is_none_or(|t| t.elapsed() >= LINK_DEAD);
            l.addr = Some(from);
            l.last_rx = Some(Instant::now());
            l.punch_until = None;
            first
        };
        if first {
            log(format!("conexão direta com um amigo ({from})"));
        }
        match msg.t.as_str() {
            "ping" => {
                let p = self.my_presence(Some(who), false);
                self.send_direct_to(who, from, "pres", Some(p));
            }
            "pres" => {
                if let Some(p) = msg.p {
                    self.got_presence(who, p);
                }
            }
            _ => {}
        }
        if first {
            self.changed();
        }
    }

    fn link_loop(self: Arc<Self>) {
        let mut was_linked: HashMap<String, bool> = HashMap::new();
        while !self.stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(2));
            let mut out: Vec<(String, SocketAddr)> = Vec::new();
            let mut changed = false;
            {
                let mut st = self.st.lock().unwrap();
                let presence = st.presence.clone();
                for (k, l) in st.links.iter_mut() {
                    let alive = l.last_rx.is_some_and(|t| t.elapsed() < LINK_DEAD);
                    if was_linked.insert(k.clone(), alive) != Some(alive) && !alive {
                        changed = true;
                    }
                    if alive {
                        if let Some(a) = l.addr.filter(|_| l.last_tx.is_none_or(|t| t.elapsed() >= LINK_PING)) {
                            out.push((k.clone(), a));
                        }
                    } else if l.punch_until.is_some_and(|t| Instant::now() < t) && l.last_tx.is_none_or(|t| t.elapsed() >= Duration::from_secs(4)) {
                        if let Some(p) = presence.get(k) {
                            for a in [&p.addr, &p.lan].iter().filter_map(|a| a.parse::<SocketAddr>().ok()) {
                                out.push((k.clone(), a));
                            }
                        }
                    }
                }
            }
            for (k, a) in out {
                self.send_direct_to(&k, a, "ping", None);
            }
            if changed {
                self.changed();
            }
        }
    }
}

/* ================================================================ funções soltas */

/// Roda as tarefas em paralelo (threads) e devolve os resultados na ordem.
fn par<T: Send + 'static, F: FnOnce() -> T + Send + 'static>(jobs: impl IntoIterator<Item = F>) -> Vec<T> {
    let handles: Vec<_> = jobs.into_iter().map(thread::spawn).collect();
    handles.into_iter().filter_map(|h| h.join().ok()).collect()
}

/// Publica em pedaços (`prefixo0`, `prefixo1`…); o primeiro byte do pedaço 0 é o total.
fn put_chunked(dht: &Dht, signer: &SigningKey, prefix: &str, data: &[u8]) -> Result<(), String> {
    let chunks: Vec<Vec<u8>> = data.chunks(CHUNK).map(|c| c.to_vec()).collect();
    if chunks.len() > MAX_CHUNKS {
        return Err(format!("grande demais ({} bytes)", data.len()));
    }
    let n = chunks.len() as u8;
    let seq = now_ms();
    let results = par(chunks.into_iter().enumerate().map(|(i, c)| {
        let (dht, signer, salt) = (dht.clone(), signer.clone(), format!("{prefix}{i}"));
        move || {
            let value = if i == 0 { [&[n][..], &c].concat() } else { c };
            dht.put_mutable(MutableItem::new(signer, &value, seq, Some(salt.as_bytes())), None).map(|_| ()).map_err(|e| e.to_string())
        }
    }));
    results.into_iter().collect()
}

fn get_chunked(dht: &Dht, pk: &[u8; 32], prefix: &str) -> Option<Vec<u8>> {
    let head = dht.get_mutable_most_recent(pk, Some(format!("{prefix}0").as_bytes()))?;
    let seq = head.seq();
    let (&n, first) = head.value().split_first()?;
    if n == 0 || n as usize > MAX_CHUNKS {
        return None;
    }
    let pk = *pk;
    let rest = par((1..n as usize).map(|i| {
        let (dht, salt) = (dht.clone(), format!("{prefix}{i}"));
        move || dht.get_mutable(&pk, Some(salt.as_bytes()), Some(seq - 1)).find(|it| it.seq() == seq).map(|it| it.value().to_vec())
    }));
    let mut out = first.to_vec();
    for r in rest {
        out.extend(r?);
    }
    Some(out)
}

/// Chave onde o aparelho novo e o principal se encontram (derivada do código de 6 dígitos).
fn link_signer(code: &str) -> SigningKey {
    SigningKey::from_bytes(&blake3::derive_key("antijanja link v1", code.as_bytes()))
}

/// Figura para conferir que o pedido achado é mesmo o do aparelho novo (3 figuras).
fn link_figure(eph: &[u8; 32], dev: &VerifyingKey) -> [(u8, u8); 3] {
    let h = blake3::derive_key("antijanja link figure v1", &[&eph[..], dev.as_bytes()].concat());
    [0, 1, 2].map(|i| (h[i] % 4, h[3 + i] % 6))
}

fn open_grant(value: &[u8], eph: &[u8; 32], code: &str) -> Option<LinkGrant> {
    if value.len() <= 32 {
        return None;
    }
    let (their, body) = value.split_at(32);
    let their: [u8; 32] = their.try_into().ok()?;
    let dh = x25519_dalek::StaticSecret::from(*eph).diffie_hellman(&x25519_dalek::PublicKey::from(their)).to_bytes();
    let key = blake3::derive_key("antijanja link seal v1", &[&dh[..], code.as_bytes()].concat());
    serde_json::from_slice(&open(&key, body)?).ok()
}

fn fetch_card(dht: &Dht, master: &VerifyingKey) -> Option<Card> {
    let item = dht.get_mutable_most_recent(master.as_bytes(), Some(b"card"))?;
    let plain = open(&card_key(master), item.value())?;
    let card: Card = serde_json::from_slice(&plain).ok()?;
    box_of(&card.b)?;
    Some(card)
}

fn open_request(id: &Identity, value: &[u8]) -> Option<Request> {
    let plain = open_sealed(id, value)?;
    let signed: Signed = serde_json::from_slice(&plain).ok()?;
    let from = master_of(signed.b.get("from")?.as_str()?)?;
    let req: Request = signed.open(&from, &[], CTX_REQUEST)?;
    let age = now_ms() - req.ts;
    (req.to == key_of(&id.master_pub()) && req.from == key_of(&from) && (-10 * 60 * 1000..REQUEST_TTL_MS).contains(&age) && box_of(&req.box_pub).is_some()).then_some(req)
}

/// Endereços onde os amigos me acham: (público, rede local), pelo socket da fofoca.
fn my_addrs() -> (String, String) {
    let public = crate::gossip::public_addr().map(|a| a.to_string()).unwrap_or_default();
    let lan = crate::gossip::local_port()
        .and_then(|port| {
            // IP da placa de rede usada para sair (nenhum pacote é mandado).
            let s = UdpSocket::bind("0.0.0.0:0").ok()?;
            s.connect("8.8.8.8:80").ok()?;
            Some(SocketAddr::new(s.local_addr().ok()?.ip(), port).to_string())
        })
        .unwrap_or_default();
    (public, lan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_box_only_opens_for_recipient() {
        let (a, b) = (Identity::create("a"), Identity::create("b"));
        let msg = b"pedido de amizade";
        let sealed = seal_to(&b.box_public(), msg);
        assert_eq!(open_sealed(&b, &sealed).as_deref(), Some(&msg[..]));
        assert!(open_sealed(&a, &sealed).is_none());
    }

    #[test]
    fn request_roundtrip_and_checks() {
        let (a, b, c) = (Identity::create("a"), Identity::create("b"), Identity::create("c"));
        let mk = |to: &Identity, ts: i64| {
            let req = Request { from: key_of(&a.master_pub()), to: key_of(&to.master_pub()), name: "Ana".into(), box_pub: b64(&a.box_public()), ts };
            let signed = Signed::new(&a, CTX_REQUEST, &req).unwrap();
            seal_to(&to.box_public(), &serde_json::to_vec(&signed).unwrap())
        };
        let v = mk(&b, now_ms());
        assert!(v.len() <= MAX_VALUE, "pedido com {} bytes", v.len());
        let r = open_request(&b, &v).unwrap();
        assert_eq!(r.name, "Ana");
        assert!(open_request(&c, &v).is_none(), "não é para ele");
        // Para outra pessoa, mas cifrado para B: recusado.
        let to_c = {
            let req = Request { from: key_of(&a.master_pub()), to: key_of(&c.master_pub()), name: "Ana".into(), box_pub: b64(&a.box_public()), ts: now_ms() };
            let signed = Signed::new(&a, CTX_REQUEST, &req).unwrap();
            seal_to(&b.box_public(), &serde_json::to_vec(&signed).unwrap())
        };
        assert!(open_request(&b, &to_c).is_none());
        assert!(open_request(&b, &mk(&b, now_ms() - REQUEST_TTL_MS - 1)).is_none(), "velho demais");
        // Alguém se passando por A (assina com outra conta): recusado.
        let fake = {
            let req = Request { from: key_of(&a.master_pub()), to: key_of(&b.master_pub()), name: "Ana".into(), box_pub: b64(&c.box_public()), ts: now_ms() };
            let signed = Signed::new(&c, CTX_REQUEST, &req).unwrap();
            seal_to(&b.box_public(), &serde_json::to_vec(&signed).unwrap())
        };
        assert!(open_request(&b, &fake).is_none());
    }

    #[test]
    fn pair_channel_keys_match_and_presence_fits() {
        let (a, b) = (Identity::create("a"), Identity::create("b"));
        let pa = Pair::new(&a, &b.master_pub(), &b.box_public());
        let pb = Pair::new(&b, &a.master_pub(), &a.box_public());
        assert_eq!(pa.cipher, pb.cipher);
        assert_eq!(pa.writer(&a.master_pub()).to_bytes(), pb.writer(&a.master_pub()).to_bytes());
        assert_ne!(pa.writer(&a.master_pub()).to_bytes(), pa.writer(&b.master_pub()).to_bytes());
        // Presença bem cheia cabe num item da DHT.
        let p = Presence {
            name: "N".repeat(MAX_NAME),
            ts: now_ms(),
            addr: "203.0.113.200:65000".into(),
            lan: "192.168.100.200:65000".into(),
            live: Some(Live { code: format!("AJ3.{}", "x".repeat(44)), title: "T".repeat(80), since: now_ms(), public: true, locked: true }),
            watch: Some(Watch { name: "W".repeat(MAX_NAME), code: format!("AJ3.{}", "y".repeat(44)) }),
            inv: Some(Invite { code: format!("AJ3.{}", "z".repeat(44)), title: "I".repeat(80), ts: now_ms() }),
            ..Default::default()
        };
        let signed = Signed::new(&a, CTX_PRESENCE, &p).unwrap();
        let value = seal(&pa.cipher, &serde_json::to_vec(&signed).unwrap());
        println!("presença: {} bytes", value.len());
        assert!(value.len() <= MAX_VALUE, "presença com {} bytes", value.len());
        let back: Signed = serde_json::from_slice(&open(&pb.cipher, &value).unwrap()).unwrap();
        assert_eq!(back.open::<Presence>(&a.master_pub(), &[], CTX_PRESENCE), Some(p.clone()));
        assert!(back.open::<Presence>(&a.master_pub(), &[signed.s.cert.dev.clone()], CTX_PRESENCE).is_none(), "aparelho desvinculado");
        assert!(back.open::<Presence>(&b.master_pub(), &[], CTX_PRESENCE).is_none());
    }
    #[test]
    fn self_sync_merges_and_respects_removals() {
        let f = |k: &str, since: i64| Friend { master: k.into(), name: k.into(), box_pub: "b".into(), since, ..Default::default() };
        let mut a = Stored::default();
        a.friends.insert("x".into(), f("x", 10));
        a.friends.insert("y".into(), f("y", 10));
        let mut b = Stored::default();
        b.friends.insert("x".into(), f("x", 10));
        b.friends.insert("z".into(), f("z", 20));
        // B desfez a amizade com Y depois que A a fez.
        b.removed.insert("y".into(), 15);
        SelfSync::of(&b).merge_into(&mut a);
        assert_eq!(a.friends.keys().cloned().collect::<Vec<_>>(), vec!["x", "z"]);
        // Refazer a amizade com Y depois da remoção vale.
        let mut c = Stored::default();
        c.friends.insert("y".into(), f("y", 30));
        SelfSync::of(&c).merge_into(&mut a);
        assert!(a.friends.contains_key("y"));
        // Juntar de novo o mesmo não muda nada.
        let before = SelfSync::of(&a);
        SelfSync::of(&a).merge_into(&mut a);
        assert_eq!(SelfSync::of(&a), before);
    }

    #[test]
    fn link_grant_opens_only_with_code_and_key() {
        let a = Identity::create("principal");
        let mut eph = [7u8; 32];
        eph[0] = 1;
        let eph_pub = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(eph)).to_bytes();
        let dev = SigningKey::from_bytes(&[9u8; 32]);
        let offer = LinkOffer { code: "123456".into(), name: "Notebook".into(), figure: link_figure(&eph_pub, &dev.verifying_key()), eph: eph_pub, dev: dev.verifying_key() };
        let cert = a.issue_cert(&offer.dev, "Notebook").unwrap();
        let grant = LinkGrant { master: key_of(&a.master_pub()), box_secret: b64(&a.box_secret_bytes()), cert, name: "Ana".into(), sync: SelfSync::default() };
        let other = x25519_dalek::StaticSecret::from([3u8; 32]);
        let other_pub = x25519_dalek::PublicKey::from(&other).to_bytes();
        let dh = other.diffie_hellman(&x25519_dalek::PublicKey::from(offer.eph)).to_bytes();
        let key = blake3::derive_key("antijanja link seal v1", &[&dh[..], offer.code.as_bytes()].concat());
        let value = [&other_pub[..], &seal(&key, &serde_json::to_vec(&grant).unwrap())].concat();
        let g = open_grant(&value, &eph, "123456").unwrap();
        let id = Identity::linked(master_of(&g.master).unwrap(), box_of(&g.box_secret).unwrap(), dev, g.cert).unwrap();
        assert_eq!(id.master_pub(), a.master_pub());
        assert_eq!(id.box_public(), a.box_public());
        assert!(open_grant(&value, &eph, "654321").is_none(), "código errado");
        assert!(open_grant(&value, &[5u8; 32], "123456").is_none(), "chave errada");
    }

    /// Duas contas viram amigas pela DHT de verdade: cartão, pedido, aceite, presença com
    /// live e convite. Leva alguns minutos: `cargo test -- --ignored friendship_over_real_dht`.
    #[test]
    #[ignore]
    fn friendship_over_real_dht() {
        let dht = Dht::client().unwrap();
        assert!(dht.bootstrapped());
        let invites = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let inv = invites.clone();
        let a = Friends::new(Identity::create("a"), "Ana", None, Box::new(|| {}), Box::new(|_, _, _| {}));
        let b = Friends::new(Identity::create("b"), "Beto", None, Box::new(|| {}), Box::new(move |name, code, _| inv.lock().unwrap().push((name, code))));
        let _ = a.dht.set(dht.clone());
        let _ = b.dht.set(dht.clone());
        let t = Instant::now();
        b.dht_round(&dht); // publica o cartão do Beto
        println!("cartão do Beto em {:?}", t.elapsed());
        let a_key = a.view().code.trim_start_matches(identity::FRIEND_PREFIX).to_string();
        let b_key = b.view().code.trim_start_matches(identity::FRIEND_PREFIX).to_string();
        a.add(&b.view().code).unwrap();
        a.dht_round(&dht);
        assert_eq!(a.view().outgoing.first().map(|r| r.name.clone()).as_deref(), Some("Beto"), "Ana não leu o cartão do Beto");
        println!("pedido enviado em {:?}", t.elapsed());

        let until = Instant::now() + Duration::from_secs(120);
        while b.view().incoming.is_empty() {
            assert!(Instant::now() < until, "pedido não chegou");
            b.st.lock().unwrap().last_inbox = None;
            b.dht_round(&dht);
        }
        assert_eq!(b.view().incoming[0].name, "Ana");
        println!("pedido recebido em {:?}", t.elapsed());
        b.set_live(Some(Live { code: "AJ3.teste".into(), title: "Jogo".into(), since: now_ms(), public: false, locked: false }));
        b.accept(&a_key).unwrap();
        b.invite(&a_key).unwrap();
        b.dht_round(&dht); // presença no canal do par = aceito

        let until = Instant::now() + Duration::from_secs(120);
        while a.view().friends.is_empty() {
            assert!(Instant::now() < until, "Ana não viu o aceite");
            a.st.lock().unwrap().last_request_put.insert(b_key.clone(), Instant::now());
            a.st.lock().unwrap().last_poll.clear();
            a.dht_round(&dht);
        }
        let f = a.view().friends[0].clone();
        println!("amizade confirmada em {:?}: {f:?}", t.elapsed());
        assert_eq!(f.name, "Beto");
        assert!(f.online);
        assert_eq!(f.live.map(|l| l.code).as_deref(), Some("AJ3.teste"));

        // E o Beto vê a Ana online.
        a.dht_round(&dht);
        let until = Instant::now() + Duration::from_secs(120);
        while !b.view().friends.first().is_some_and(|f| f.online) {
            assert!(Instant::now() < until, "Beto não viu a Ana online");
            b.st.lock().unwrap().last_poll.clear();
            b.dht_round(&dht);
        }
        println!("presença dos dois lados em {:?}", t.elapsed());
        let _ = invites;
    }
    /// Um aparelho novo é vinculado pelo principal pela DHT de verdade e recebe a conta e a
    /// lista de amigos: `cargo test -- --ignored link_over_real_dht`.
    #[test]
    #[ignore]
    fn link_over_real_dht() {
        let dht = Dht::client().unwrap();
        assert!(dht.bootstrapped());
        let a = Friends::new(Identity::create("principal"), "Ana", None, Box::new(|| {}), Box::new(|_, _, _| {}));
        a.st.lock().unwrap().stored.friends.insert("amigo".into(), Friend { master: "amigo".into(), name: "Carla".into(), box_pub: "b".into(), since: 5, ..Default::default() });
        let b = Friends::new(Identity::create("novo"), "Ana", None, Box::new(|| {}), Box::new(|_, _, _| {}));
        let _ = a.dht.set(dht.clone());
        let _ = b.dht.set(dht.clone());
        let t = Instant::now();
        b.link_request("Notebook");
        let wait = b.view().link.unwrap();
        // O pedido leva alguns segundos para estar na DHT.
        let until = Instant::now() + Duration::from_secs(90);
        let offer = loop {
            match a.link_lookup(&wait.code) {
                Ok(o) => break o,
                Err(e) => {
                    assert!(Instant::now() < until, "pedido não achado: {e}");
                    thread::sleep(Duration::from_secs(2));
                }
            }
        };
        println!("pedido achado em {:?}", t.elapsed());
        assert_eq!(offer.figure, wait.figure, "a figura tem que bater");
        assert_eq!(offer.name, "Notebook");
        a.link_confirm(&offer, "").unwrap();
        let until = Instant::now() + Duration::from_secs(90);
        while b.view().code != a.view().code {
            assert!(Instant::now() < until, "aparelho novo não recebeu a conta: {:?}", b.view().link);
            thread::sleep(Duration::from_millis(500));
        }
        println!("vinculado em {:?}", t.elapsed());
        let v = b.view();
        assert!(!v.primary);
        assert_eq!(v.device_name, "Notebook");
        assert!(b.st.lock().unwrap().stored.friends.contains_key("amigo"), "lista de amigos não veio");
        assert_eq!(a.view().devices.len(), 2);
    }
}
