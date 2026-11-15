//! Encontro dos peers sem servidor, usando a DHT Mainline do BitTorrent (BEP44).
//!
//! O código da transmissão (`AJ2.<seed>`) carrega 32 bytes aleatórios. Dele derivamos:
//! - a chave de assinatura do host (onde ele publica respostas);
//! - a chave de assinatura dos espectadores (onde eles publicam pedidos);
//! - a chave de criptografia (ChaCha20-Poly1305) de todas as mensagens.
//!
//! Os nós da DHT só veem blobs criptografados e assinados. Quem não tem o código
//! não consegue ler nem forjar mensagens. Depois do encontro, o vídeo vai direto
//! entre os peers via WebRTC; a DHT não participa mais.
//!
//! Protocolo (WebRTC: o espectador oferece, o host responde):
//! 1. O espectador escolhe um slot `k`, publica sua oferta em `viewer_key / salt "v{k}.{i}"`.
//! 2. O host consulta os slots continuamente e, ao achar uma oferta nova, gera a resposta.
//! 3. O host publica a resposta em `host_key / salt "h{k}.{i}"`; o espectador a busca.
//!
//! Mensagens maiores que o limite do BEP44 (1000 bytes) são divididas em pedaços;
//! o pedaço 0 traz um cabeçalho com o total e é publicado por último.

#![allow(deprecated)] // usamos a API bloqueante do mainline, em threads próprias

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use mainline::{Dht, MutableItem, SigningKey};
use serde::Serialize;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use crate::app::AppHandle;

const CODE_PREFIX: &str = "AJ2.";
pub const SLOTS: u8 = 8;
const CHUNK: usize = 900;
const MAX_CHUNKS: usize = 8;
const MAX_PLAIN: u64 = 64 * 1024;
const NONCE_LEN: usize = 12;

#[derive(Default)]
pub struct RdvState {
    dht: OnceLock<Dht>,
    hosting: Mutex<Option<Arc<AtomicBool>>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Host,
    Viewer,
}

impl Role {
    fn tag(self) -> char {
        match self {
            Role::Host => 'h',
            Role::Viewer => 'v',
        }
    }
}

struct Keys {
    host: SigningKey,
    viewer: SigningKey,
    cipher: ChaCha20Poly1305,
}

impl Keys {
    fn from_code(code: &str) -> Result<Self, String> {
        let cleaned: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        let body = cleaned
            .strip_prefix(CODE_PREFIX)
            .ok_or("Esse não parece ser um código do AntiJanja.")?;
        let seed: [u8; 32] = URL_SAFE_NO_PAD
            .decode(body)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or("Código incompleto ou digitado errado.")?;
        Ok(Self::from_seed(&seed))
    }

    fn from_seed(seed: &[u8; 32]) -> Self {
        let derive = |ctx: &str| blake3::derive_key(ctx, seed);
        Keys {
            host: SigningKey::from_bytes(&derive("antijanja v2 host signing key")),
            viewer: SigningKey::from_bytes(&derive("antijanja v2 viewer signing key")),
            cipher: ChaCha20Poly1305::new_from_slice(&derive("antijanja v2 encryption key"))
                .expect("chave de 32 bytes"),
        }
    }

    fn signer(&self, role: Role) -> &SigningKey {
        match role {
            Role::Host => &self.host,
            Role::Viewer => &self.viewer,
        }
    }

    fn seal(&self, plain: &str) -> Result<Vec<u8>, String> {
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::best());
        enc.write_all(plain.as_bytes()).map_err(|e| e.to_string())?;
        let compressed = enc.finish().map_err(|e| e.to_string())?;
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
        let mut out = nonce.to_vec();
        out.extend(
            self.cipher
                .encrypt(&Nonce::from(nonce), compressed.as_slice())
                .map_err(|_| "falha ao criptografar")?,
        );
        Ok(out)
    }

    fn open(&self, sealed: &[u8]) -> Option<String> {
        if sealed.len() <= NONCE_LEN {
            return None;
        }
        let (nonce, body) = sealed.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().ok()?;
        let compressed = self.cipher.decrypt(&Nonce::from(nonce), body).ok()?;
        let mut out = String::new();
        DeflateDecoder::new(compressed.as_slice())
            .take(MAX_PLAIN)
            .read_to_string(&mut out)
            .ok()?;
        Some(out)
    }
}

fn salt(role: Role, slot: u8, chunk: usize) -> Vec<u8> {
    format!("{}{}.{}", role.tag(), slot, chunk).into_bytes()
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Maior `seq` já publicado nos primeiros `chunks` pedaços de um slot (0 se vazio).
///
/// Vários espectadores dividem os slots (são só 8). Se alguém com o relógio adiantado
/// usou o mesmo slot antes, um `seq = agora` nosso seria *menor* que o dele: os nós da
/// DHT recusam o item e o host (que só aceita `seq` maior que o último visto) nunca vê
/// a oferta, e o espectador fica "esperando o host" para sempre.
fn current_seq(dht: &Dht, keys: &Keys, role: Role, slot: u8, chunks: usize) -> i64 {
    let pk = keys.signer(role).verifying_key().to_bytes();
    let handles: Vec<_> = (0..chunks)
        .map(|i| {
            let (dht, s) = (dht.clone(), salt(role, slot, i));
            thread::spawn(move || dht.get_mutable_most_recent(&pk, Some(&s)).map(|it| it.seq()).unwrap_or(0))
        })
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok()).max().unwrap_or(0)
}

/// Publica uma mensagem (criptografada, dividida em pedaços) num slot.
/// O `seq` usado é sempre maior que `min_seq` (além de `agora`).
fn publish(dht: &Dht, keys: &Keys, role: Role, slot: u8, payload: &str, min_seq: i64) -> Result<i64, String> {
    let sealed = keys.seal(payload)?;
    let chunks: Vec<&[u8]> = sealed.chunks(CHUNK).collect();
    if chunks.len() > MAX_CHUNKS {
        return Err("Mensagem de conexão grande demais.".into());
    }
    let seq = now_ms().max(min_seq + 1);
    let signer = keys.signer(role);

    // Todos os pedaços em paralelo. Se alguém ler o cabeçalho antes dos demais
    // pedaços chegarem, `fetch` falha e simplesmente tenta de novo depois.
    let handles: Vec<_> = chunks
        .iter()
        .enumerate()
        .map(|(i, data)| {
            let mut value = Vec::with_capacity(data.len() + 1);
            if i == 0 {
                value.push(chunks.len() as u8); // cabeçalho: total de pedaços
            }
            value.extend_from_slice(data);
            let (dht, signer, s) = (dht.clone(), signer.clone(), salt(role, slot, i));
            thread::spawn(move || {
                dht.put_mutable(MutableItem::new(signer, &value, seq, Some(&s)), None)
                    .map_err(|e| format!("Falha ao publicar na rede P2P: {e}"))
            })
        })
        .collect();
    for h in handles {
        h.join().map_err(|_| "thread de publicação falhou")??;
    }
    Ok(seq)
}

/// Busca a mensagem mais nova que `newer_than` num slot. Retorna (seq, texto).
fn fetch(dht: &Dht, keys: &Keys, role: Role, slot: u8, newer_than: i64) -> Option<(i64, String)> {
    let pk = keys.signer(role).verifying_key().to_bytes();
    let head = dht
        .get_mutable(&pk, Some(&salt(role, slot, 0)), Some(newer_than))
        .filter(|item| item.seq() > newer_than)
        .max_by_key(|item| item.seq())?;
    let seq = head.seq();
    let (&total, first) = head.value().split_first()?;
    let total = total as usize;
    if total == 0 || total > MAX_CHUNKS {
        return None;
    }

    let rest: Vec<_> = (1..total)
        .map(|i| {
            let (dht, s) = (dht.clone(), salt(role, slot, i));
            thread::spawn(move || {
                dht.get_mutable(&pk, Some(&s), Some(seq - 1))
                    .find(|item| item.seq() == seq)
                    .map(|item| item.value().to_vec())
            })
        })
        .collect();

    let mut sealed = first.to_vec();
    for h in rest {
        sealed.extend(h.join().ok()??);
    }
    keys.open(&sealed).map(|text| (seq, text))
}

fn get_dht(state: &RdvState) -> Result<Dht, String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(dht) = state.dht.get() {
            return Ok(dht.clone());
        }
        if Instant::now() > deadline {
            return Err("Não foi possível entrar na rede P2P. Verifique sua internet.".into());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn raise_stop(slot: &Mutex<Option<Arc<AtomicBool>>>) {
    if let Some(flag) = slot.lock().unwrap().take() {
        flag.store(true, Ordering::SeqCst);
    }
}

/// Inicia o nó da DHT em segundo plano assim que o app abre.
pub fn init(app: &AppHandle) {
    let app = app.clone();
    thread::spawn(move || match Dht::client() {
        Ok(dht) => {
            let ok = dht.bootstrapped();
            let _ = app.rdv().dht.set(dht);
            let _ = app.emit("dht-status", ok);
        }
        Err(e) => {
            eprintln!("DHT indisponível: {e}");
            let _ = app.emit("dht-status", false);
        }
    });
}

/// Cria um novo código e começa a escutar pedidos de espectadores.
/// `on_offer(slot, payload)` é chamado para cada pedido novo.
pub fn host_listen(state: &RdvState, on_offer: Arc<dyn Fn(u8, String) + Send + Sync>) -> Result<String, String> {
    raise_stop(&state.hosting);
    let dht = get_dht(state)?;

    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| e.to_string())?;
    let code = format!("{CODE_PREFIX}{}", URL_SAFE_NO_PAD.encode(seed));
    let keys = Arc::new(Keys::from_seed(&seed));

    let stop = Arc::new(AtomicBool::new(false));
    *state.hosting.lock().unwrap() = Some(stop.clone());

    // Uma thread por slot, cada uma consultando a DHT em loop.
    for slot in 0..SLOTS {
        let (dht, keys, stop, on_offer) = (dht.clone(), keys.clone(), stop.clone(), on_offer.clone());
        thread::spawn(move || {
            let mut last_seq = 0i64;
            while !stop.load(Ordering::SeqCst) {
                let started = Instant::now();
                if let Some((seq, payload)) = fetch(&dht, &keys, Role::Viewer, slot, last_seq) {
                    last_seq = seq;
                    if !stop.load(Ordering::SeqCst) {
                        on_offer(slot, payload);
                    }
                }
                // Intervalo mínimo entre consultas, para não sobrecarregar a rede.
                let min = Duration::from_millis(1500);
                if let Some(rest) = min.checked_sub(started.elapsed()) {
                    thread::sleep(rest);
                }
            }
        });
    }
    Ok(code)
}

pub fn host_stop(state: &RdvState) {
    raise_stop(&state.hosting);
}

/// Host: publica a resposta WebRTC para o espectador no slot indicado.
pub fn host_reply(state: &RdvState, code: &str, slot: u8, payload: &str) -> Result<(), String> {
    let keys = Keys::from_code(code)?;
    let dht = get_dht(state)?;
    // Só este host escreve nas chaves deste código, mas o relógio pode voltar
    // (ajuste do Windows): mantemos o seq sempre crescente.
    static LAST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let min = LAST.load(Ordering::SeqCst);
    let seq = publish(&dht, &keys, Role::Host, slot % SLOTS, payload, min)?;
    LAST.fetch_max(seq, Ordering::SeqCst);
    Ok(())
}

/// Espectador: publica a oferta e espera a resposta do host (até `timeout_secs`).
/// `sid` identifica esta tentativa; respostas com outro `sid` são ignoradas.
pub fn join(
    state: &RdvState,
    code: &str,
    sid: &str,
    payload: &str,
    timeout_secs: u64,
    stop: &AtomicBool,
    on_published: impl Fn(),
) -> Result<String, String> {
    // Cada espectador tem a própria flag de cancelamento: várias entradas podem
    // acontecer ao mesmo tempo (assistir várias transmissões).
    let keys = Keys::from_code(code)?;
    let dht = get_dht(state)?;

    let mut r = [0u8; 1];
    getrandom::fill(&mut r).map_err(|e| e.to_string())?;
    let slot = r[0] % SLOTS;

    let mut last_seq = 0i64;
    let mut published_at: Option<Instant> = None;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.clamp(10, 600));
    // O slot pode ter sido usado por outro espectador (com outro relógio).
    let mut my_seq = current_seq(&dht, &keys, Role::Viewer, slot, MAX_CHUNKS);

    while Instant::now() < deadline {
        if stop.load(Ordering::SeqCst) {
            return Err("cancelado".into());
        }
        // Republica a oferta periodicamente, caso o host tenha perdido a primeira.
        if published_at.is_none_or(|t| t.elapsed() > Duration::from_secs(25)) {
            let first = published_at.is_none();
            my_seq = publish(&dht, &keys, Role::Viewer, slot, payload, my_seq)?;
            published_at = Some(Instant::now());
            if first {
                on_published();
            }
        }
        if let Some((seq, text)) = fetch(&dht, &keys, Role::Host, slot, last_seq) {
            last_seq = seq;
            let matches = serde_json::from_str::<serde_json::Value>(&text)
                .map(|v| v["sid"] == sid)
                .unwrap_or(false);
            if matches {
                return Ok(text);
            }
        }
        thread::sleep(Duration::from_millis(700));
    }
    Err("O host não respondeu. Confira se o código está certo e se a transmissão ainda está ativa.".into())
}

/// A DHT já está pronta para uso?
pub fn rdv_ready(state: &RdvState) -> bool {
    state.dht.get().is_some()
}

/* ========================================================================== */
/*                 Lista pública de transmissões (sem servidor)                 */
/* ========================================================================== */
//
// Um "mural" com PUBLIC_SLOTS vagas na DHT, assinado por uma chave que todo AntiJanja
// conhece (derivada de uma frase fixa). Quem transmite em modo público ocupa uma vaga
// livre e atualiza a cada minuto; quem está no Início lê todas as vagas.
// Por ser público, qualquer um pode escrever nas vagas: as entradas são validadas e as
// velhas (> 3 min sem atualizar) são ignoradas.

pub const PUBLIC_SLOTS: u8 = 48;
const PUBLIC_TTL_MS: i64 = 3 * 60 * 1000;
const PUBLIC_REFRESH: Duration = Duration::from_secs(60);

fn public_key() -> SigningKey {
    SigningKey::from_bytes(&blake3::derive_key("antijanja public directory v1", b"mural de transmissoes ao vivo"))
}

fn public_salt(slot: u8) -> Vec<u8> {
    format!("pub{slot}").into_bytes()
}

/// Identificador desta execução do app (para reconhecer a própria entrada).
pub fn instance_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let mut b = [0u8; 8];
        let _ = getrandom::fill(&mut b);
        b.iter().map(|x| format!("{x:02x}")).collect()
    })
}

#[derive(Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct PublicEntry {
    pub id: String,
    pub name: String,
    pub title: String,
    pub code: String,
    pub viewers: u32,
    pub quality: String,
    /// Início da transmissão (ms desde 1970).
    pub since: i64,
    /// Última atualização (ms desde 1970).
    pub ts: i64,
    #[serde(default)]
    pub gone: bool,
}

#[derive(Serialize, Clone)]
pub struct PublicStream {
    #[serde(flatten)]
    pub entry: PublicEntry,
    pub own: bool,
}

fn valid(e: &PublicEntry) -> bool {
    !e.gone
        && e.code.starts_with(CODE_PREFIX)
        && e.code.len() < 80
        && Keys::from_code(&e.code).is_ok()
        && !e.name.trim().is_empty()
        && e.name.chars().count() <= 40
        && e.title.chars().count() <= 80
        && e.viewers < 10_000
        && (now_ms() - e.ts).abs() < PUBLIC_TTL_MS
}

fn fetch_slot(dht: &Dht, slot: u8) -> Option<(i64, PublicEntry)> {
    let pk = public_key().verifying_key().to_bytes();
    let item = dht.get_mutable_most_recent(&pk, Some(&public_salt(slot)))?;
    let entry: PublicEntry = serde_json::from_slice(item.value()).ok()?;
    Some((item.seq(), entry))
}

/// Lê todas as vagas (em paralelo). Devolve (vaga, entrada) das válidas e recentes.
fn fetch_directory(dht: &Dht) -> Vec<(u8, PublicEntry)> {
    let handles: Vec<_> = (0..PUBLIC_SLOTS)
        .map(|slot| {
            let dht = dht.clone();
            thread::spawn(move || fetch_slot(&dht, slot).map(|(_, e)| (slot, e)))
        })
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
}

fn put_slot(dht: &Dht, slot: u8, entry: &PublicEntry) -> Result<(), String> {
    let value = serde_json::to_vec(entry).map_err(|e| e.to_string())?;
    dht.put_mutable(MutableItem::new(public_key(), &value, now_ms(), Some(&public_salt(slot))), None)
        .map(|_| ())
        .map_err(|e| format!("Falha ao publicar na lista: {e}"))
}

/// Lista as transmissões públicas ao vivo agora.
pub fn public_list(state: &RdvState) -> Result<Vec<PublicStream>, String> {
    let dht = get_dht(state)?;
    let me = instance_id();
    let mut seen = std::collections::HashSet::new();
    let mut list: Vec<PublicStream> = fetch_directory(&dht)
        .into_iter()
        .map(|(_, e)| e)
        .filter(valid)
        .filter(|e| seen.insert(e.code.clone())) // a mesma transmissão em duas vagas
        .map(|e| PublicStream { own: e.id == me, entry: e })
        .collect();
    list.sort_by(|a, b| b.entry.viewers.cmp(&a.entry.viewers).then(a.entry.since.cmp(&b.entry.since)));
    Ok(list)
}

/// Mantém a transmissão anunciada no mural enquanto estiver ativa.
pub struct PublicAnnouncer {
    entry: Arc<Mutex<PublicEntry>>,
    stop: Arc<AtomicBool>,
    wake: std::sync::mpsc::Sender<()>,
}

impl PublicAnnouncer {
    pub fn start(state: &RdvState, entry: PublicEntry) -> Result<Self, String> {
        let dht = get_dht(state)?;
        let entry = Arc::new(Mutex::new(entry));
        let stop = Arc::new(AtomicBool::new(false));
        let (wake, wake_rx) = std::sync::mpsc::channel::<()>();
        let (e, s) = (entry.clone(), stop.clone());
        thread::Builder::new()
            .name("aj-public".into())
            .spawn(move || {
                let me = instance_id().to_string();
                let mut slot: Option<u8> = None;
                while !s.load(Ordering::SeqCst) {
                    // Escolhe (ou confere) a vaga: livre, velha ou já nossa.
                    let current = slot.and_then(|sl| fetch_slot(&dht, sl));
                    let ours = matches!(&current, Some((_, en)) if en.id == me || !valid(en));
                    if slot.is_none() || !ours {
                        let taken: std::collections::HashSet<u8> = fetch_directory(&dht)
                            .into_iter()
                            .filter(|(_, en)| valid(en) && en.id != me)
                            .map(|(sl, _)| sl)
                            .collect();
                        let mut free: Vec<u8> = (0..PUBLIC_SLOTS).filter(|sl| !taken.contains(sl)).collect();
                        if free.is_empty() {
                            free = (0..PUBLIC_SLOTS).collect(); // mural cheio: disputa uma vaga
                        }
                        let mut r = [0u8; 1];
                        let _ = getrandom::fill(&mut r);
                        slot = Some(free[r[0] as usize % free.len()]);
                    }
                    let mut en = e.lock().unwrap().clone();
                    en.ts = now_ms();
                    en.id = me.clone();
                    let _ = put_slot(&dht, slot.unwrap(), &en);
                    // Espera o próximo ciclo (ou uma mudança importante).
                    let _ = wake_rx.recv_timeout(PUBLIC_REFRESH);
                }
                // Saindo: limpa a vaga para sumir da lista na hora.
                if let Some(sl) = slot {
                    let mut en = e.lock().unwrap().clone();
                    en.gone = true;
                    en.ts = now_ms();
                    let _ = put_slot(&dht, sl, &en);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { entry, stop, wake })
    }

    /// Atualiza os dados anunciados. `now` = publica já (ex.: código novo).
    pub fn update(&self, f: impl FnOnce(&mut PublicEntry), now: bool) {
        f(&mut self.entry.lock().unwrap());
        if now {
            let _ = self.wake.send(());
        }
    }
}

impl Drop for PublicAnnouncer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.wake.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_roundtrip_and_crypto() {
        let seed = [7u8; 32];
        let code = format!("{CODE_PREFIX}{}", URL_SAFE_NO_PAD.encode(seed));
        let a = Keys::from_seed(&seed);
        let b = Keys::from_code(&format!("  {code}\n")).unwrap();
        assert_eq!(a.host.to_bytes(), b.host.to_bytes());
        assert_ne!(a.host.to_bytes(), a.viewer.to_bytes());

        let msg = r#"{"t":"offer","sdp":"v=0\r\n..."}"#;
        let sealed = a.seal(msg).unwrap();
        assert_eq!(b.open(&sealed).as_deref(), Some(msg));

        let other = Keys::from_seed(&[8u8; 32]);
        assert!(other.open(&sealed).is_none());
    }

    /// Teste contra a DHT real (precisa de internet): `cargo test -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_dht_roundtrip() {
        let dht = Dht::client().unwrap();
        assert!(dht.bootstrapped(), "falha no bootstrap da DHT");
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).unwrap();
        let keys = Keys::from_seed(&seed);
        // ~2,7 KB pouco compressível => vários pedaços, como um SDP real.
        let mut noise = [0u8; 2000];
        getrandom::fill(&mut noise).unwrap();
        let payload = URL_SAFE_NO_PAD.encode(noise);

        let t = Instant::now();
        let seq = publish(&dht, &keys, Role::Viewer, 3, &payload, 0).unwrap();
        println!("publish: {:?}", t.elapsed());
        let t = Instant::now();
        let (got_seq, got) = fetch(&dht, &keys, Role::Viewer, 3, 0).expect("não achou na DHT");
        println!("fetch: {:?}", t.elapsed());
        assert_eq!(got_seq, seq);
        assert_eq!(got, payload);
        assert!(fetch(&dht, &keys, Role::Viewer, 3, seq).is_none());
    }

    /// Simula host + espectador ao mesmo tempo na DHT real.
    #[test]
    #[ignore]
    fn live_full_handshake() {
        let dht = Dht::client().unwrap();
        assert!(dht.bootstrapped());
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).unwrap();
        let keys = Arc::new(Keys::from_seed(&seed));
        let start = Instant::now();

        // Host: uma thread por slot, como em rdv_host_start.
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        for slot in 0..SLOTS {
            let (dht, keys, tx, stop) = (dht.clone(), keys.clone(), tx.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    if let Some((_, offer)) = fetch(&dht, &keys, Role::Viewer, slot, 0) {
                        let _ = tx.send((slot, offer));
                        return;
                    }
                    thread::sleep(Duration::from_millis(1500));
                }
            });
        }
        // Espectador publica no slot 5.
        {
            let (dht, keys) = (dht.clone(), keys.clone());
            thread::spawn(move || publish(&dht, &keys, Role::Viewer, 5, r#"{"t":"offer","sid":"abc"}"#, 0).unwrap());
        }
        let (slot, offer) = rx.recv_timeout(Duration::from_secs(60)).expect("host não achou a oferta");
        stop.store(true, Ordering::SeqCst);
        println!("host recebeu a oferta em {:?}", start.elapsed());
        assert_eq!(slot, 5);
        assert!(offer.contains("abc"));

        publish(&dht, &keys, Role::Host, slot, r#"{"t":"answer","sid":"abc"}"#, 0).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some((_, answer)) = fetch(&dht, &keys, Role::Host, 5, 0) {
                assert!(answer.contains("answer"));
                break;
            }
            assert!(Instant::now() < deadline, "espectador não achou a resposta");
        }
        println!("handshake completo em {:?}", start.elapsed());
    }

    #[test]
    fn rejects_bad_codes() {
        assert!(Keys::from_code("hello").is_err());
        assert!(Keys::from_code("AJ2.abc").is_err());
    }

    /// Anuncia no mural público e confere se a entrada aparece na leitura.
    #[test]
    #[ignore]
    fn live_public_directory() {
        let state = RdvState::default();
        let dht = Dht::client().unwrap();
        assert!(dht.bootstrapped());
        let _ = state.dht.set(dht.clone());
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).unwrap();
        let code = format!("{CODE_PREFIX}{}", URL_SAFE_NO_PAD.encode(seed));
        let entry = PublicEntry {
            id: "teste".into(), name: "Teste AntiJanja".into(), title: "Tela 1".into(), code: code.clone(),
            viewers: 2, quality: "1080p60".into(), since: now_ms(), ts: now_ms(), gone: false,
        };
        let t = Instant::now();
        let ann = PublicAnnouncer::start(&state, entry).unwrap();
        let mut found = false;
        for _ in 0..6 {
            thread::sleep(Duration::from_secs(4));
            let list = fetch_directory(&dht);
            println!("{:?}: {} vagas com algo, {} válidas", t.elapsed(), list.len(), list.iter().filter(|(_, e)| valid(e)).count());
            if list.iter().any(|(_, e)| e.code == code && valid(e)) { found = true; break; }
        }
        assert!(found, "a entrada não apareceu no mural");
        println!("apareceu no mural em {:?}", t.elapsed());
        drop(ann);
        thread::sleep(Duration::from_secs(8));
        let still = fetch_directory(&dht).into_iter().any(|(_, e)| e.code == code && valid(&e));
        println!("depois de parar, ainda listada: {still}");
        assert!(!still);
    }
}
