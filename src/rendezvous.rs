//! Encontro dos peers sem servidor, usando a DHT Mainline do BitTorrent (BEP44).
//!
//! O host sorteia um par de chaves Ed25519; o código da transmissão (`AJ3.<chave pública>`)
//! é a chave pública dele. Do código derivamos:
//! - a chave de assinatura dos espectadores (onde eles publicam pedidos);
//! - a chave de criptografia (ChaCha20-Poly1305) de todas as mensagens.
//!
//! A chave *privada* do host nunca sai do computador dele: só ele publica respostas e
//! entradas no mural. Quem tem o código consegue pedir para assistir, mas não consegue
//! se passar pelo host (no formato antigo, `AJ2.`, a chave do host saía do código e
//! qualquer um com o código podia responder no lugar dele).
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
use ed25519_dalek::{Signature, Signer, VerifyingKey};
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use mainline::{Dht, MutableItem, SigningKey};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use crate::app::AppHandle;

pub const CODE_PREFIX: &str = "AJ3.";
const OLD_PREFIX: &str = "AJ2.";
pub const SLOTS: u8 = 8;
const CHUNK: usize = 900;
const MAX_CHUNKS: usize = 8;
const MAX_PLAIN: u64 = 64 * 1024;
const NONCE_LEN: usize = 12;
/// `seq` (ms desde 1970) mais adiantado que isso em relação ao nosso relógio é tratado
/// como lixo: quem tem o código poderia publicar `seq = i64::MAX` e travar um slot
/// (os nós da DHT recusam qualquer `seq` menor e o host pararia de olhar o slot).
const MAX_FUTURE_MS: i64 = 10 * 60 * 1000;

/// Esse `seq` é plausível (não está muito no futuro)?
fn sane_seq(seq: i64) -> bool {
    seq <= now_ms().saturating_add(MAX_FUTURE_MS)
}

/// Chaves dos códigos que este app gerou (com a chave privada do host), por código.
type HostKeys = Arc<Mutex<HashMap<String, Arc<Keys>>>>;

#[derive(Default)]
pub struct RdvState {
    dht: OnceLock<Dht>,
    hosting: Mutex<Option<Arc<AtomicBool>>>,
    /// O código atual e os anteriores da mesma transmissão (quem pediu para entrar
    /// pouco antes de um "gerar novo código" ainda recebe a resposta).
    host_keys: HostKeys,
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

/// Confere um código colado e devolve a chave pública do host que ele carrega.
fn parse_code(code: &str) -> Result<VerifyingKey, String> {
    let cleaned: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.starts_with(OLD_PREFIX) {
        return Err("Esse código é de uma versão antiga do AntiJanja. Peça para quem está transmitindo atualizar o app.".into());
    }
    let body = cleaned
        .strip_prefix(CODE_PREFIX)
        .ok_or(format!("Esse não é um código do AntiJanja. Ele começa com {CODE_PREFIX}"))?;
    let bytes: [u8; 32] = URL_SAFE_NO_PAD
        .decode(body)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("Código incompleto ou digitado errado.")?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| "Código incompleto ou digitado errado.".to_string())
}

/// O código parece válido? (para avisar na interface antes de tentar entrar)
pub fn check_code(code: &str) -> Result<(), String> {
    parse_code(code).map(|_| ())
}

struct Keys {
    /// Chave pública do host (é o próprio código).
    host_pk: [u8; 32],
    /// Chave privada do host: só existe no computador de quem transmite.
    host: Option<SigningKey>,
    viewer: SigningKey,
    cipher: ChaCha20Poly1305,
}

impl Keys {
    /// Espectador: chaves a partir do código colado.
    fn from_code(code: &str) -> Result<Self, String> {
        Ok(Self::derive(parse_code(code)?.to_bytes(), None))
    }

    /// Host: sorteia um par de chaves novo. Devolve (código, chaves).
    fn new_host() -> Result<(String, Self), String> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| e.to_string())?;
        let sk = SigningKey::from_bytes(&seed);
        let pk = sk.verifying_key().to_bytes();
        Ok((format!("{CODE_PREFIX}{}", URL_SAFE_NO_PAD.encode(pk)), Self::derive(pk, Some(sk))))
    }

    fn derive(host_pk: [u8; 32], host: Option<SigningKey>) -> Self {
        let derive = |ctx: &str| blake3::derive_key(ctx, &host_pk);
        Keys {
            host_pk,
            host,
            viewer: SigningKey::from_bytes(&derive("antijanja v3 viewer signing key")),
            cipher: ChaCha20Poly1305::new_from_slice(&derive("antijanja v3 encryption key"))
                .expect("chave de 32 bytes"),
        }
    }

    /// Chave pública onde `role` publica.
    fn public(&self, role: Role) -> [u8; 32] {
        match role {
            Role::Host => self.host_pk,
            Role::Viewer => self.viewer.verifying_key().to_bytes(),
        }
    }

    fn signer(&self, role: Role) -> Result<&SigningKey, String> {
        match role {
            Role::Host => self.host.as_ref().ok_or_else(|| "só o host publica respostas".to_string()),
            Role::Viewer => Ok(&self.viewer),
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
    let pk = keys.public(role);
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
    let seq = now_ms().max(min_seq.saturating_add(1));
    let signer = keys.signer(role)?;

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
    let pk = keys.public(role);
    let head = dht
        .get_mutable(&pk, Some(&salt(role, slot, 0)), Some(newer_than))
        .filter(|item| item.seq() > newer_than && sane_seq(item.seq()))
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

/// Recebe (código em que o pedido chegou, slot, pedido).
pub type OfferFn = Arc<dyn Fn(&str, u8, String) + Send + Sync>;

/// Cria um novo código e começa a escutar pedidos de espectadores.
/// `on_offer` é chamado para cada pedido novo.
pub fn host_listen(state: &RdvState, on_offer: OfferFn) -> Result<String, String> {
    raise_stop(&state.hosting);
    let dht = get_dht(state)?;

    let (code, keys) = Keys::new_host()?;
    let keys = Arc::new(keys);
    state.host_keys.lock().unwrap().insert(code.clone(), keys.clone());

    let stop = Arc::new(AtomicBool::new(false));
    *state.hosting.lock().unwrap() = Some(stop.clone());

    // Uma thread por slot, cada uma consultando a DHT em loop.
    for slot in 0..SLOTS {
        let (dht, keys, stop, on_offer) = (dht.clone(), keys.clone(), stop.clone(), on_offer.clone());
        let code = code.clone();
        thread::spawn(move || {
            let mut last_seq = 0i64;
            while !stop.load(Ordering::SeqCst) {
                let started = Instant::now();
                if let Some((seq, payload)) = fetch(&dht, &keys, Role::Viewer, slot, last_seq) {
                    last_seq = seq;
                    if !stop.load(Ordering::SeqCst) {
                        on_offer(&code, slot, payload);
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

/// Para de escutar e esquece as chaves (os códigos antigos deixam de funcionar).
pub fn host_stop(state: &RdvState) {
    raise_stop(&state.hosting);
    state.host_keys.lock().unwrap().clear();
}

/// Host: publica a resposta WebRTC para o espectador no slot indicado.
pub fn host_reply(state: &RdvState, code: &str, slot: u8, payload: &str) -> Result<(), String> {
    let keys = state.host_keys.lock().unwrap().get(code).cloned().ok_or("código que não é desta transmissão")?;
    let dht = get_dht(state)?;
    // Só este host escreve nas chaves deste código, mas o relógio pode voltar
    // (ajuste do Windows): mantemos o seq sempre crescente.
    static LAST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let min = LAST.load(Ordering::SeqCst);
    let seq = publish(&dht, &keys, Role::Host, slot % SLOTS, payload, min)?;
    LAST.fetch_max(seq, Ordering::SeqCst);
    Ok(())
}

/// Escolhe um slot (começando por um sorteado) que não esteja travado por um `seq`
/// absurdo, nem do lado do espectador nem da resposta do host. Devolve (slot, seq atual).
fn pick_slot(dht: &Dht, keys: &Keys) -> Result<(u8, i64), String> {
    let mut r = [0u8; 1];
    getrandom::fill(&mut r).map_err(|e| e.to_string())?;
    for i in 0..SLOTS {
        let slot = r[0].wrapping_add(i) % SLOTS;
        // O slot pode ter sido usado por outro espectador (com outro relógio).
        let (viewer, host) = thread::scope(|s| {
            let host = s.spawn(|| current_seq(dht, keys, Role::Host, slot, MAX_CHUNKS));
            let viewer = current_seq(dht, keys, Role::Viewer, slot, MAX_CHUNKS);
            (viewer, host.join().unwrap_or(0))
        });
        if sane_seq(viewer) && sane_seq(host) {
            return Ok((slot, viewer));
        }
    }
    Err("Não foi possível pedir para entrar: a rede está bloqueada para este código. Peça um código novo ao host.".into())
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

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.clamp(10, 600));
    let (slot, mut my_seq) = pick_slot(&dht, &keys)?;
    let mut last_seq = 0i64;
    let mut published_at: Option<Instant> = None;

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
// Um "mural" com PUBLIC_SLOTS vagas na DHT, gravável por uma chave que todo AntiJanja
// conhece (derivada de uma frase fixa). Quem transmite em modo público ocupa uma vaga
// livre e atualiza a cada minuto; quem está no Início lê todas as vagas.
//
// Como qualquer um pode escrever nas vagas, o mural se defende assim:
// - cada entrada é assinada pela chave privada do host do código anunciado: ninguém
//   anuncia, encerra ou muda o nome de uma transmissão que não é sua;
// - as vagas mudam de endereço a cada PUBLIC_EPOCH_MS: quem travar uma vaga com um
//   `seq` absurdo só a trava até a próxima troca (e vagas travadas são evitadas);
// - de cada transmissão vale só a entrada mais nova, e as velhas (> 3 min) são ignoradas.
// O que não dá para impedir sem servidor: alguém sobrescrever vagas sem parar.

pub const PUBLIC_SLOTS: u8 = 48;
const PUBLIC_TTL_MS: i64 = 3 * 60 * 1000;
const PUBLIC_REFRESH: Duration = Duration::from_secs(60);
const PUBLIC_EPOCH_MS: i64 = 10 * 60 * 1000;
/// No começo de um período também lemos o anterior (relógios um pouco diferentes e
/// hosts que ainda não republicaram no período novo).
const PUBLIC_OVERLAP_MS: i64 = 90 * 1000;
/// Separa as assinaturas do mural de qualquer outro uso da chave do host.
const MURAL_CONTEXT: &[u8] = b"antijanja mural v2:";

fn public_key() -> SigningKey {
    SigningKey::from_bytes(&blake3::derive_key("antijanja public directory v2", b"mural de transmissoes ao vivo"))
}

fn public_salt(epoch: i64, slot: u8) -> Vec<u8> {
    format!("pub2.{epoch}.{slot}").into_bytes()
}

fn epoch_of(ms: i64) -> i64 {
    ms.div_euclid(PUBLIC_EPOCH_MS)
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
        && e.code.len() < 80
        && parse_code(&e.code).is_ok()
        && !e.name.trim().is_empty()
        && e.name.chars().count() <= 40
        && e.title.chars().count() <= 80
        && e.viewers < 10_000
        && (now_ms() - e.ts).abs() < PUBLIC_TTL_MS
}

/// Assina a entrada com a chave do host do código anunciado: `assinatura (64) || json`.
fn seal_entry(keys: &Keys, entry: &PublicEntry) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(entry).map_err(|e| e.to_string())?;
    let sig = keys.signer(Role::Host)?.sign(&[MURAL_CONTEXT, &json].concat());
    Ok([&sig.to_bytes()[..], &json].concat())
}

/// Lê uma entrada do mural, só se a assinatura bater com o código anunciado.
fn open_entry(value: &[u8]) -> Option<PublicEntry> {
    if value.len() <= Signature::BYTE_SIZE {
        return None;
    }
    let (sig, json) = value.split_at(Signature::BYTE_SIZE);
    let entry: PublicEntry = serde_json::from_slice(json).ok()?;
    let sig = Signature::from_slice(sig).ok()?;
    parse_code(&entry.code).ok()?.verify_strict(&[MURAL_CONTEXT, json].concat(), &sig).ok()?;
    Some(entry)
}

/// O que há numa vaga.
struct SlotRead {
    /// Alguém publicou um `seq` absurdo aqui: escrever nela tende a falhar.
    jammed: bool,
    /// A entrada (com assinatura válida) mais recente.
    entry: Option<PublicEntry>,
}

fn fetch_slot(dht: &Dht, epoch: i64, slot: u8) -> SlotRead {
    let pk = public_key().verifying_key().to_bytes();
    let mut items: Vec<MutableItem> = dht.get_mutable(&pk, Some(&public_salt(epoch, slot)), None).collect();
    let jammed = items.iter().any(|it| !sane_seq(it.seq()));
    items.retain(|it| sane_seq(it.seq()));
    items.sort_by_key(|it| std::cmp::Reverse(it.seq()));
    SlotRead { jammed, entry: items.iter().find_map(|it| open_entry(it.value())) }
}

/// Lê todas as vagas de um período (em paralelo).
fn fetch_directory(dht: &Dht, epoch: i64) -> Vec<(u8, SlotRead)> {
    let handles: Vec<_> = (0..PUBLIC_SLOTS)
        .map(|slot| {
            let dht = dht.clone();
            thread::spawn(move || (slot, fetch_slot(&dht, epoch, slot)))
        })
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok()).collect()
}

fn put_slot(dht: &Dht, epoch: i64, slot: u8, value: &[u8]) -> Result<(), String> {
    dht.put_mutable(MutableItem::new(public_key(), value, now_ms(), Some(&public_salt(epoch, slot))), None)
        .map(|_| ())
        .map_err(|e| format!("Falha ao publicar na lista: {e}"))
}

/// A entrada mais nova de cada transmissão. Assim uma entrada antiga reaproveitada por
/// alguém (mesmo com assinatura válida) não esconde um "encerrou" mais novo.
fn latest_per_code(entries: impl IntoIterator<Item = PublicEntry>) -> Vec<PublicEntry> {
    let mut by_code: HashMap<String, PublicEntry> = HashMap::new();
    for e in entries {
        if by_code.get(&e.code).is_none_or(|old| e.ts > old.ts) {
            by_code.insert(e.code.clone(), e);
        }
    }
    by_code.into_values().collect()
}

/// Lista as transmissões públicas ao vivo agora.
pub fn public_list(state: &RdvState) -> Result<Vec<PublicStream>, String> {
    let dht = get_dht(state)?;
    let now = now_ms();
    let epoch = epoch_of(now);
    let reads = thread::scope(|s| {
        let previous = (now - epoch * PUBLIC_EPOCH_MS < PUBLIC_OVERLAP_MS).then(|| s.spawn(|| fetch_directory(&dht, epoch - 1)));
        let mut reads = fetch_directory(&dht, epoch);
        if let Some(p) = previous {
            reads.extend(p.join().unwrap_or_default());
        }
        reads
    });
    let me = instance_id();
    let mut list: Vec<PublicStream> = latest_per_code(reads.into_iter().filter_map(|(_, r)| r.entry))
        .into_iter()
        .filter(valid)
        .map(|e| PublicStream { own: e.id == me, entry: e })
        .collect();
    list.sort_by(|a, b| b.entry.viewers.cmp(&a.entry.viewers).then(a.entry.since.cmp(&b.entry.since)));
    Ok(list)
}

/// Vaga para anunciar: a mesma de antes se ainda for nossa (ou estiver livre);
/// senão uma livre e não travada, sorteada.
fn choose_slot(dht: &Dht, epoch: i64, place: Option<(i64, u8)>, me: &str) -> u8 {
    let mine_or_free = |r: &SlotRead| !r.jammed && r.entry.as_ref().is_none_or(|en| en.id == me || !valid(en));
    if let Some((ep, sl)) = place {
        if ep == epoch && mine_or_free(&fetch_slot(dht, epoch, sl)) {
            return sl;
        }
    }
    let dir = fetch_directory(dht, epoch);
    if let Some((sl, _)) = dir.iter().find(|(_, r)| !r.jammed && r.entry.as_ref().is_some_and(|en| en.id == me)) {
        return *sl;
    }
    let pick = |f: &dyn Fn(&SlotRead) -> bool| -> Vec<u8> { dir.iter().filter(|(_, r)| f(r)).map(|(sl, _)| *sl).collect() };
    let mut usable = pick(&mine_or_free);
    if usable.is_empty() {
        usable = pick(&|r: &SlotRead| !r.jammed); // mural cheio: disputa uma vaga
    }
    if usable.is_empty() {
        usable = (0..PUBLIC_SLOTS).collect();
    }
    let mut r = [0u8; 2];
    let _ = getrandom::fill(&mut r);
    usable[u16::from_le_bytes(r) as usize % usable.len()]
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
        let host_keys = state.host_keys.clone();
        let entry = Arc::new(Mutex::new(entry));
        let stop = Arc::new(AtomicBool::new(false));
        let (wake, wake_rx) = std::sync::mpsc::channel::<()>();
        let (e, s) = (entry.clone(), stop.clone());
        thread::Builder::new()
            .name("aj-public".into())
            .spawn(move || {
                let me = instance_id().to_string();
                // (período, vaga) onde estamos anunciados.
                let mut place: Option<(i64, u8)> = None;
                // Chave do código anunciado. Fica guardada aqui: ao encerrar, o app já
                // esqueceu as chaves, mas ainda precisamos assinar o "encerrou".
                let mut signer: Option<(String, Arc<Keys>)> = None;
                while !s.load(Ordering::SeqCst) {
                    let mut en = e.lock().unwrap().clone();
                    if signer.as_ref().is_none_or(|(code, _)| *code != en.code) {
                        signer = host_keys.lock().unwrap().get(&en.code).map(|k| (en.code.clone(), k.clone()));
                    }
                    let epoch = epoch_of(now_ms());
                    if let Some((_, keys)) = &signer {
                        let slot = choose_slot(&dht, epoch, place, &me);
                        place = Some((epoch, slot));
                        en.ts = now_ms();
                        en.id = me.clone();
                        if let Ok(value) = seal_entry(keys, &en) {
                            let _ = put_slot(&dht, epoch, slot, &value);
                        }
                    }
                    // Próximo ciclo: a cada minuto, logo depois da troca de período
                    // ou numa mudança importante.
                    let to_next_epoch = (epoch + 1) * PUBLIC_EPOCH_MS - now_ms() + 2_000;
                    let _ = wake_rx.recv_timeout(PUBLIC_REFRESH.min(Duration::from_millis(to_next_epoch.max(0) as u64)));
                }
                // Saindo: marca a entrada como encerrada para sumir da lista na hora.
                if let (Some((epoch, slot)), Some((code, keys))) = (place, &signer) {
                    let mut en = e.lock().unwrap().clone();
                    en.code = code.clone();
                    en.gone = true;
                    en.ts = now_ms();
                    en.id = me.clone();
                    if let Ok(value) = seal_entry(keys, &en) {
                        let _ = put_slot(&dht, epoch, slot, &value);
                    }
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

    fn host() -> (String, Keys) {
        Keys::new_host().unwrap()
    }

    #[test]
    fn code_roundtrip_and_crypto() {
        let (code, a) = host();
        assert!(code.starts_with(CODE_PREFIX));
        let b = Keys::from_code(&format!("  {code}\n")).unwrap();
        assert_eq!(a.host_pk, b.host_pk);
        assert_eq!(a.viewer.to_bytes(), b.viewer.to_bytes());
        assert_ne!(a.public(Role::Host), a.public(Role::Viewer));

        let msg = r#"{"t":"offer","sdp":"v=0\r\n..."}"#;
        let sealed = a.seal(msg).unwrap();
        assert_eq!(b.open(&sealed).as_deref(), Some(msg));

        let (_, other) = host();
        assert!(other.open(&sealed).is_none());
    }

    /// Quem só tem o código não consegue assinar como o host.
    #[test]
    fn viewer_cannot_sign_as_host() {
        let (code, a) = host();
        let b = Keys::from_code(&code).unwrap();
        assert!(a.signer(Role::Host).is_ok());
        assert!(b.signer(Role::Host).is_err());
        assert!(b.signer(Role::Viewer).is_ok());
    }

    #[test]
    fn rejects_bad_codes() {
        assert!(Keys::from_code("hello").is_err());
        assert!(Keys::from_code("AJ3.abc").is_err());
        let old = format!("{OLD_PREFIX}{}", URL_SAFE_NO_PAD.encode([7u8; 32]));
        assert!(check_code(&old).unwrap_err().contains("versão antiga"));
    }

    fn entry(code: &str, ts: i64, gone: bool) -> PublicEntry {
        PublicEntry {
            id: "x".into(), name: "Fulano".into(), title: "Jogo".into(), code: code.into(),
            viewers: 1, quality: "1080p60".into(), since: ts, ts, gone,
        }
    }

    #[test]
    fn mural_entries_must_be_signed_by_the_host() {
        let (code, keys) = host();
        let e = entry(&code, now_ms(), false);
        let sealed = seal_entry(&keys, &e).unwrap();
        assert_eq!(open_entry(&sealed), Some(e.clone()));

        // Alterar qualquer coisa (aqui o nome) invalida a assinatura.
        let mut tampered = sealed.clone();
        let at = tampered.windows(6).position(|w| w == b"Fulano").unwrap();
        tampered[at] = b'B';
        assert_eq!(open_entry(&tampered), None);

        // Outro host assinando uma entrada com o código desta transmissão.
        let (_, attacker) = host();
        assert_eq!(open_entry(&seal_entry(&attacker, &e).unwrap()), None);

        // Quem só tem o código nem consegue assinar.
        assert!(seal_entry(&Keys::from_code(&code).unwrap(), &e).is_err());
        assert_eq!(open_entry(b"lixo"), None);
    }

    #[test]
    fn newest_entry_per_stream_wins() {
        let (code, _) = host();
        let t = now_ms();
        let list = latest_per_code([entry(&code, t - 30_000, false), entry(&code, t, true), entry(&code, t - 10_000, false)]);
        assert_eq!(list.len(), 1);
        assert!(list[0].gone, "um \"encerrou\" mais novo não pode ser escondido por uma entrada antiga");
    }

    #[test]
    fn rejects_absurd_seq() {
        assert!(sane_seq(now_ms()));
        assert!(sane_seq(now_ms() + 60_000)); // relógio um pouco adiantado
        assert!(!sane_seq(now_ms() + MAX_FUTURE_MS + 60_000));
        assert!(!sane_seq(i64::MAX));
    }

    /// Teste contra a DHT real (precisa de internet): `cargo test -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_dht_roundtrip() {
        let dht = Dht::client().unwrap();
        assert!(dht.bootstrapped(), "falha no bootstrap da DHT");
        let (_, keys) = host();
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
        let (code, host_keys) = host();
        let host_keys = Arc::new(host_keys);
        let viewer_keys = Arc::new(Keys::from_code(&code).unwrap());
        let start = Instant::now();

        // Host: uma thread por slot, como em host_listen.
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        for slot in 0..SLOTS {
            let (dht, keys, tx, stop) = (dht.clone(), host_keys.clone(), tx.clone(), stop.clone());
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
        // Espectador (só com o código) publica no slot 5.
        {
            let (dht, keys) = (dht.clone(), viewer_keys.clone());
            thread::spawn(move || publish(&dht, &keys, Role::Viewer, 5, r#"{"t":"offer","sid":"abc"}"#, 0).unwrap());
        }
        let (slot, offer) = rx.recv_timeout(Duration::from_secs(60)).expect("host não achou a oferta");
        stop.store(true, Ordering::SeqCst);
        println!("host recebeu a oferta em {:?}", start.elapsed());
        assert_eq!(slot, 5);
        assert!(offer.contains("abc"));

        publish(&dht, &host_keys, Role::Host, slot, r#"{"t":"answer","sid":"abc"}"#, 0).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some((_, answer)) = fetch(&dht, &viewer_keys, Role::Host, 5, 0) {
                assert!(answer.contains("answer"));
                break;
            }
            assert!(Instant::now() < deadline, "espectador não achou a resposta");
        }
        println!("handshake completo em {:?}", start.elapsed());
    }

    /// O caminho do app: host_listen -> join -> host_reply (com a chave guardada no estado).
    #[test]
    #[ignore]
    fn live_listen_join_reply() {
        let dht = Dht::client().unwrap();
        assert!(dht.bootstrapped());
        let host = Arc::new(RdvState::default());
        let viewer = RdvState::default();
        let _ = host.dht.set(dht.clone());
        let _ = viewer.dht.set(dht);
        let h = host.clone();
        let on_offer: OfferFn = Arc::new(move |code: &str, slot: u8, offer: String| {
            let sid = serde_json::from_str::<serde_json::Value>(&offer).unwrap()["sid"].as_str().unwrap().to_string();
            host_reply(&h, code, slot, &serde_json::json!({ "t": "answer", "sid": sid }).to_string()).unwrap();
        });
        let code = host_listen(&host, on_offer).unwrap();
        let t = Instant::now();
        let answer = join(&viewer, &code, "s1", r#"{"t":"offer","sid":"s1"}"#, 90, &AtomicBool::new(false), || {}).unwrap();
        println!("resposta em {:?}", t.elapsed());
        assert!(answer.contains("answer"));
        host_stop(&host);
        assert!(host_reply(&host, &code, 0, "{}").is_err(), "depois de parar, o código não responde mais");
    }

    /// Trava 7 dos 8 slots (seq absurdo) e confere que o espectador escolhe o que sobrou.
    /// Pelo lado da resposta só o próprio host conseguiria travar agora, mas o teste
    /// cobre os dois lados. Usa um código novo, de ninguém.
    #[test]
    #[ignore]
    fn live_skips_poisoned_slots() {
        let dht = Dht::client().unwrap();
        assert!(dht.bootstrapped());
        let (_, keys) = host();
        let handles: Vec<_> = (0..SLOTS)
            .filter(|&s| s != 5)
            .map(|slot| {
                let role = if slot % 2 == 0 { Role::Viewer } else { Role::Host };
                let (dht, key) = (dht.clone(), keys.signer(role).unwrap().clone());
                thread::spawn(move || dht.put_mutable(MutableItem::new(key, b"lixo", i64::MAX - 1, Some(&salt(role, slot, 0))), None))
            })
            .collect();
        for h in handles {
            h.join().unwrap().unwrap();
        }
        let (slot, _) = pick_slot(&dht, &keys).expect("devia achar o slot livre");
        assert_eq!(slot, 5);
    }

    /// Anuncia no mural público e confere se a entrada aparece na leitura e some ao parar.
    #[test]
    #[ignore]
    fn live_public_directory() {
        let state = RdvState::default();
        let dht = Dht::client().unwrap();
        assert!(dht.bootstrapped());
        let _ = state.dht.set(dht.clone());
        let (code, keys) = host();
        state.host_keys.lock().unwrap().insert(code.clone(), Arc::new(keys));
        let t = Instant::now();
        let ann = PublicAnnouncer::start(&state, entry(&code, now_ms(), false)).unwrap();
        let mut found = false;
        for _ in 0..6 {
            thread::sleep(Duration::from_secs(4));
            let list = public_list(&state).unwrap();
            println!("{:?}: {} transmissões na lista", t.elapsed(), list.len());
            if list.iter().any(|s| s.entry.code == code && s.own) {
                found = true;
                break;
            }
        }
        assert!(found, "a entrada não apareceu no mural");
        println!("apareceu no mural em {:?}", t.elapsed());
        host_stop(&state); // como o app faz: esquece as chaves antes de parar o anúncio
        drop(ann);
        thread::sleep(Duration::from_secs(8));
        let still = public_list(&state).unwrap().iter().any(|s| s.entry.code == code);
        println!("depois de parar, ainda listada: {still}");
        assert!(!still);
    }
}
