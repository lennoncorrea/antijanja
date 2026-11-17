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

/// Marcador no 33º byte do código: a transmissão tem senha.
const FLAG_PASSWORD: u8 = 1;

/// Confere um código colado. Devolve a chave pública do host e se a transmissão tem senha.
fn parse_code(code: &str) -> Result<(VerifyingKey, bool), String> {
    let cleaned: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.starts_with(OLD_PREFIX) {
        return Err("Esse código é de uma versão antiga do AntiJanja. Peça para quem está transmitindo atualizar o app.".into());
    }
    let body = cleaned
        .strip_prefix(CODE_PREFIX)
        .ok_or(format!("Esse não é um código do AntiJanja. Ele começa com {CODE_PREFIX}"))?;
    let bad = || "Código incompleto ou digitado errado.".to_string();
    let raw = URL_SAFE_NO_PAD.decode(body).map_err(|_| bad())?;
    // 32 bytes: sem senha. 33: o último é um marcador.
    let locked = match raw.len() {
        32 => false,
        33 if raw[32] == FLAG_PASSWORD => true,
        _ => return Err(bad()),
    };
    let pk: [u8; 32] = raw[..32].try_into().map_err(|_| bad())?;
    Ok((VerifyingKey::from_bytes(&pk).map_err(|_| bad())?, locked))
}

/// O código parece válido? Devolve se a transmissão pede senha.
pub fn check_code(code: &str) -> Result<bool, String> {
    parse_code(code).map(|(_, locked)| locked)
}

/// Segredo tirado da senha. Argon2id (64 MiB, 3 passadas): lento de propósito, para
/// testar senhas uma a uma ficar caro mesmo para quem tem o código. O sal é a chave
/// pública do host (cada transmissão tem a sua).
fn password_secret(password: &str, host_pk: &[u8; 32]) -> Result<[u8; 32], String> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(64 * 1024, 3, 1, Some(32)).map_err(|e| e.to_string())?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), host_pk, &mut out)
        .map_err(|e| e.to_string())?;
    Ok(out)
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
    /// Espectador: chaves a partir do código colado (e da senha, se a transmissão tiver).
    fn from_code(code: &str, password: Option<&str>) -> Result<Self, String> {
        let (pk, locked) = parse_code(code)?;
        let pk = pk.to_bytes();
        let secret = match (locked, password.filter(|p| !p.is_empty())) {
            (false, _) => None,
            (true, Some(p)) => Some(password_secret(p, &pk)?),
            (true, None) => return Err("Essa transmissão tem senha.".into()),
        };
        Ok(Self::derive(pk, secret, None))
    }

    /// Host: sorteia um par de chaves novo. Devolve (código, chaves).
    fn new_host(password: Option<&str>) -> Result<(String, Self), String> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| e.to_string())?;
        let sk = SigningKey::from_bytes(&seed);
        let pk = sk.verifying_key().to_bytes();
        let password = password.filter(|p| !p.is_empty());
        let mut raw = pk.to_vec();
        if password.is_some() {
            raw.push(FLAG_PASSWORD);
        }
        let secret = password.map(|p| password_secret(p, &pk)).transpose()?;
        Ok((format!("{CODE_PREFIX}{}", URL_SAFE_NO_PAD.encode(raw)), Self::derive(pk, secret, Some(sk))))
    }

    /// Com senha, as chaves dependem também do segredo dela: sem a senha não dá para
    /// ler nem mandar mensagens, nem tendo o código.
    fn derive(host_pk: [u8; 32], secret: Option<[u8; 32]>, host: Option<SigningKey>) -> Self {
        let mut material = host_pk.to_vec();
        if let Some(s) = secret {
            material.extend_from_slice(&s);
        }
        let derive = |ctx: &str| blake3::derive_key(ctx, &material);
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

/// Refaz a busca dos nós que guardam um slot. Depois da primeira publicação num endereço,
/// a biblioteca da DHT guarda os nós mais próximos dele e publica direto neles nas vezes
/// seguintes. Se esse grupo veio ruim (busca que não chegou nos nós certos), todas as
/// republicações iam para os mesmos nós e o outro lado nunca via a mensagem (visto num
/// teste: 6 respostas seguidas do host, nenhuma recebida). Uma consulta nova troca o grupo.
/// Devolve o maior `seq` encontrado no slot.
fn refresh_nodes(dht: &Dht, keys: &Keys, role: Role, slot: u8) -> i64 {
    let t = Instant::now();
    let seq = current_seq(dht, keys, role, slot, MAX_CHUNKS);
    let who = if role == Role::Host { "host" } else { "espectador" };
    crate::connlog::log(format!("{who}: nós do slot {slot} buscados de novo em {} ms", t.elapsed().as_millis()));
    seq
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
            start_gossip(&app, &dht);
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
pub fn host_listen(state: &RdvState, on_offer: OfferFn, password: Option<&str>) -> Result<String, String> {
    raise_stop(&state.hosting);
    let dht = get_dht(state)?;

    let (code, keys) = Keys::new_host(password)?;
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

/// Tentativas de publicar uma resposta do host antes de desistir (o espectador ainda
/// republica o pedido de tempos em tempos, e aí a resposta vai de novo).
const REPLY_TRIES: u32 = 3;

/// Uma resposta do host por vez: duas no mesmo slot ao mesmo tempo, a biblioteca da DHT
/// recusa a segunda enquanto a primeira está saindo ("risco de conflito").
static REPLY_LOCK: Mutex<()> = Mutex::new(());

/// Host: publica a resposta WebRTC (ou o aviso de ocupado) para o espectador no slot
/// indicado. Tenta de novo se a publicação falhar.
///
/// Sempre busca os nós do slot na hora (`refresh_nodes`), em vez de publicar nos que a
/// biblioteca guardou de uma resposta anterior no mesmo slot. Os guardados saem uns 3 s
/// mais rápido, mas nos testes às vezes a resposta ia para nós que o espectador não
/// encontra, e ele ficava em "Esperando o host responder" até desistir.
pub fn host_reply(state: &RdvState, code: &str, slot: u8, payload: &str) -> Result<(), String> {
    let keys = state.host_keys.lock().unwrap().get(code).cloned().ok_or("código que não é desta transmissão")?;
    let dht = get_dht(state)?;
    // Só este host escreve nas chaves deste código, mas o relógio pode voltar
    // (ajuste do Windows): mantemos o seq sempre crescente.
    static LAST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let slot = slot % SLOTS;
    let mut last_err = String::new();
    for attempt in 1..=REPLY_TRIES {
        let t = Instant::now();
        let result = {
            let _one_at_a_time = REPLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            refresh_nodes(&dht, &keys, Role::Host, slot);
            publish(&dht, &keys, Role::Host, slot, payload, LAST.load(Ordering::SeqCst))
        };
        match result {
            Ok(seq) => {
                LAST.fetch_max(seq, Ordering::SeqCst);
                crate::connlog::log(format!("host: resposta publicada no slot {slot} em {} ms (tentativa {attempt})", t.elapsed().as_millis()));
                return Ok(());
            }
            Err(e) => {
                crate::connlog::log(format!("host: falha ao publicar a resposta no slot {slot} (tentativa {attempt}): {e}"));
                last_err = e;
            }
        }
        if attempt < REPLY_TRIES {
            thread::sleep(Duration::from_millis(1500 * attempt as u64));
        }
    }
    Err(last_err)
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

/// O espectador republica o pedido nesse intervalo, caso o host não o tenha visto (a
/// DHT perde ou demora a propagar) ou tenha respondido que estava ocupado.
const REPUBLISH: Duration = Duration::from_secs(12);

/// O que acontece durante `join`, para a interface.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum JoinEvent {
    /// O pedido foi publicado pela primeira vez.
    Published,
    /// O host está ocupado com outras entradas; o pedido vai de novo sozinho.
    Busy,
}

/// O que dá para preparar antes de ter o pedido (chaves, DHT e slot). Escolher o slot
/// consulta a DHT e leva alguns segundos: o espectador faz isso enquanto o WebRTC monta a
/// oferta, e não depois.
pub struct JoinPrep {
    keys: Keys,
    dht: Dht,
    slot: u8,
    seq: i64,
}

/// Espectador, primeira parte da entrada: chaves do código (com a senha, que é lenta de
/// propósito), DHT pronta e slot escolhido.
pub fn prepare_join(state: &RdvState, code: &str, password: Option<&str>) -> Result<JoinPrep, String> {
    use crate::connlog::{log, short};
    let started = Instant::now();
    let keys = Keys::from_code(code, password)?;
    let dht = get_dht(state)?;
    let (slot, seq) = pick_slot(&dht, &keys).inspect_err(|e| log(format!("entrada no código {}: sem slot: {e}", short(code))))?;
    log(format!("entrada no código {}: slot {slot} escolhido em {} ms", short(code), started.elapsed().as_millis()));
    Ok(JoinPrep { keys, dht, slot, seq })
}

/// Espectador, segunda parte: publica a oferta e espera a resposta do host (até
/// `timeout_secs`). `sid` identifica esta tentativa; respostas com outro `sid` são
/// ignoradas. Se desistir (cancelado ou sem resposta), avisa o host para ele liberar a vaga.
pub fn join(prep: JoinPrep, sid: &str, payload: &str, timeout_secs: u64, stop: &AtomicBool, on_event: impl Fn(JoinEvent)) -> Result<String, String> {
    use crate::connlog::{log, short};
    // Cada espectador tem a própria flag de cancelamento: várias entradas podem
    // acontecer ao mesmo tempo (assistir várias transmissões).
    let JoinPrep { keys, dht, slot, seq: mut my_seq } = prep;
    let started = Instant::now();
    let tag = format!("espectador {}", short(sid));
    log(format!("{tag}: mandando o pedido ({} bytes) no slot {slot}", payload.len()));

    let deadline = started + Duration::from_secs(timeout_secs.clamp(10, 600));
    let mut last_seq = 0i64;
    let mut published_at: Option<Instant> = None;
    let mut publishes = 0u32;
    let mut busy = false;

    let err = loop {
        if stop.load(Ordering::SeqCst) {
            break "cancelado".to_string();
        }
        if Instant::now() >= deadline {
            break "O host não respondeu. Confira se o código está certo e se a transmissão ainda está ativa.".to_string();
        }
        // Republica a oferta periodicamente, caso o host tenha perdido a anterior.
        if published_at.is_none_or(|t| t.elapsed() > REPUBLISH) {
            // O host não respondeu à publicação anterior: talvez ela tenha ido para nós
            // ruins. Busca os nós de novo (e o seq mais novo do slot) antes de repetir.
            if publishes > 0 {
                let cur = refresh_nodes(&dht, &keys, Role::Viewer, slot);
                if sane_seq(cur) {
                    my_seq = my_seq.max(cur);
                }
            }
            let t = Instant::now();
            match publish(&dht, &keys, Role::Viewer, slot, payload, my_seq) {
                Ok(seq) => {
                    my_seq = seq;
                    publishes += 1;
                    log(format!("{tag}: pedido publicado (vez {publishes}) em {} ms", t.elapsed().as_millis()));
                    if publishes == 1 {
                        on_event(JoinEvent::Published);
                    }
                }
                Err(e) => {
                    log(format!("{tag}: falha ao publicar o pedido: {e}"));
                    // Na primeira vez nada saiu: é erro. Depois, tenta de novo mais tarde.
                    if publishes == 0 {
                        break e;
                    }
                }
            }
            published_at = Some(Instant::now());
        }
        if let Some((seq, text)) = fetch(&dht, &keys, Role::Host, slot, last_seq) {
            last_seq = seq;
            let v = serde_json::from_str::<serde_json::Value>(&text).unwrap_or_default();
            if v["sid"] != sid {
                log(format!("{tag}: resposta de outra tentativa no slot (ignorada)"));
            } else if v["t"] == "busy" {
                log(format!("{tag}: host ocupado com outras entradas"));
                if !busy {
                    busy = true;
                    on_event(JoinEvent::Busy);
                }
            } else {
                log(format!("{tag}: resposta recebida em {} ms", started.elapsed().as_millis()));
                return Ok(text);
            }
        }
        thread::sleep(Duration::from_millis(700));
    };
    log(format!("{tag}: desistiu depois de {} s ({err}); pedido publicado {publishes} vez(es)", started.elapsed().as_secs()));
    // Avisa o host (em segundo plano) para ele não guardar a vaga desta tentativa.
    if publishes > 0 {
        let cancel = serde_json::json!({ "t": "cancel", "sid": sid }).to_string();
        thread::spawn(move || {
            let _ = publish(&dht, &keys, Role::Viewer, slot, &cancel, my_seq);
        });
    }
    Err(err)
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
    let phrase = format!("mural de transmissoes ao vivo{}", test_net().map(|n| format!(" (teste {n})")).unwrap_or_default());
    SigningKey::from_bytes(&blake3::derive_key("antijanja public directory v2", phrase.as_bytes()))
}

/// Rede de teste isolada (`AJ_TEST_NET`, só na versão de desenvolvimento): o mural e a
/// fofoca usam outra chave e outro tópico, invisíveis para os apps de verdade. Testes de
/// transmissão pública só aqui (o autoteste transmite a tela de quem roda).
pub fn test_net() -> Option<&'static str> {
    static NET: OnceLock<Option<String>> = OnceLock::new();
    NET.get_or_init(|| if cfg!(debug_assertions) { std::env::var("AJ_TEST_NET").ok().filter(|s| !s.is_empty()) } else { None }).as_deref()
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
    /// A transmissão pede senha (cadeado na lista).
    #[serde(default)]
    pub locked: bool,
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
        // O cadeado tem que bater com o código (ninguém anuncia "sem senha" errado).
        && parse_code(&e.code).is_ok_and(|(_, locked)| locked == e.locked)
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
    parse_code(&entry.code).ok()?.0.verify_strict(&[MURAL_CONTEXT, json].concat(), &sig).ok()?;
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
    sort_public(&mut list);
    Ok(list)
}

/// Mais espectadores primeiro; empate, a mais antiga.
fn sort_public(list: &mut [PublicStream]) {
    list.sort_by(|a, b| b.entry.viewers.cmp(&a.entry.viewers).then(a.entry.since.cmp(&b.entry.since)));
}

/// Junta na lista um anúncio que chegou pela fofoca: vale o mais novo de cada transmissão;
/// encerrada (ou velha) sai. Devolve se a lista mudou.
pub fn merge_public(list: &mut Vec<PublicStream>, e: &PublicEntry) -> bool {
    if list.iter().any(|s| s.entry.code == e.code && s.entry.ts >= e.ts) {
        return false;
    }
    let before = list.len();
    list.retain(|s| s.entry.code != e.code);
    let removed = list.len() != before;
    if valid(e) {
        list.push(PublicStream { own: e.id == instance_id(), entry: e.clone() });
        sort_public(list);
        return true;
    }
    removed
}

/// O anúncio ainda vale pelo horário (os velhos saem da lista)?
pub fn entry_alive(e: &PublicEntry) -> bool {
    (now_ms() - e.ts).abs() < PUBLIC_TTL_MS
}

/* ========================================================================== */
/*                  Fofoca entre os apps (docs/gossip-ao-vivo.md)               */
/* ========================================================================== */

/// Confere um anúncio do mural vindo da fofoca: assinatura do host do código anunciado e
/// horário recente (o "encerrou" também passa). Chave: o código.
fn gossip_check(value: &[u8]) -> Option<(String, i64)> {
    let e = open_entry(value)?;
    (e.code.len() < 80 && entry_alive(&e)).then(|| (e.code.clone(), e.ts))
}

/// Liga a fofoca aos anúncios do mural: cada anúncio novo vira o evento "public-entry".
fn start_gossip(app: &AppHandle, dht: &Dht) {
    let app = app.clone();
    let cfg = crate::gossip::Config {
        id: instance_id().to_string(),
        net: test_net().unwrap_or_default().to_string(),
        dht: Some(dht.clone()),
        lan: true,
        bind: std::net::Ipv4Addr::UNSPECIFIED,
        check: Box::new(gossip_check),
        deliver: Box::new(move |value| {
            if let Some(e) = open_entry(value) {
                let what = if e.gone { "encerrou" } else if e.id == instance_id() { "nosso" } else { "ao vivo" };
                crate::connlog::log(format!("fofoca: anúncio {} ({what})", crate::connlog::short(&e.code)));
                let _ = app.emit("public-entry", e);
            }
        }),
    };
    match crate::gossip::Gossip::start(cfg) {
        Ok(g) => crate::gossip::install(g),
        Err(e) => crate::connlog::log(format!("fofoca: não iniciou: {e}")),
    }
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
                        en.ts = now_ms();
                        en.id = me.clone();
                        if let Ok(value) = seal_entry(keys, &en) {
                            // Na fofoca primeiro: chega na hora a quem está conectado; o
                            // mural na DHT leva alguns segundos (escolher a vaga lê o mural).
                            crate::gossip::broadcast(&value);
                            let slot = choose_slot(&dht, epoch, place, &me);
                            place = Some((epoch, slot));
                            let _ = put_slot(&dht, epoch, slot, &value);
                        }
                    }
                    // Próximo ciclo: a cada minuto, logo depois da troca de período
                    // ou numa mudança importante.
                    let to_next_epoch = (epoch + 1) * PUBLIC_EPOCH_MS - now_ms() + 2_000;
                    let _ = wake_rx.recv_timeout(PUBLIC_REFRESH.min(Duration::from_millis(to_next_epoch.max(0) as u64)));
                }
                // Saindo: marca a entrada como encerrada para sumir da lista na hora.
                if let Some((code, keys)) = &signer {
                    let mut en = e.lock().unwrap().clone();
                    en.code = code.clone();
                    en.gone = true;
                    en.ts = now_ms();
                    en.id = me.clone();
                    if let Ok(value) = seal_entry(keys, &en) {
                        crate::gossip::broadcast(&value);
                        if let Some((epoch, slot)) = place {
                            let _ = put_slot(&dht, epoch, slot, &value);
                        }
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
        Keys::new_host(None).unwrap()
    }

    #[test]
    fn code_roundtrip_and_crypto() {
        let (code, a) = host();
        assert!(code.starts_with(CODE_PREFIX));
        let b = Keys::from_code(&format!("  {code}\n"), None).unwrap();
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
        let b = Keys::from_code(&code, None).unwrap();
        assert!(a.signer(Role::Host).is_ok());
        assert!(b.signer(Role::Host).is_err());
        assert!(b.signer(Role::Viewer).is_ok());
    }

    #[test]
    fn rejects_bad_codes() {
        assert!(Keys::from_code("hello", None).is_err());
        assert!(Keys::from_code("AJ3.abc", None).is_err());
        let old = format!("{OLD_PREFIX}{}", URL_SAFE_NO_PAD.encode([7u8; 32]));
        assert!(check_code(&old).unwrap_err().contains("versão antiga"));
    }

    fn entry(code: &str, ts: i64, gone: bool) -> PublicEntry {
        PublicEntry {
            id: "x".into(), name: "Fulano".into(), title: "Jogo".into(), code: code.into(),
            viewers: 1, quality: "1080p60".into(), since: ts, ts, gone, locked: false,
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
        assert!(seal_entry(&Keys::from_code(&code, None).unwrap(), &e).is_err());
        assert_eq!(open_entry(b"lixo"), None);
    }

    #[test]
    fn gossip_entries_merge_into_the_list() {
        let (code, keys) = host();
        let mut e = entry(&code, now_ms(), false);
        e.name = "Ana".into();
        let mut list = Vec::new();
        assert!(merge_public(&mut list, &e), "anúncio novo entra");
        assert_eq!(list.len(), 1);
        assert!(!merge_public(&mut list, &e), "repetido não muda nada");
        let mut older = e.clone();
        older.ts -= 1000;
        older.name = "Velho".into();
        assert!(!merge_public(&mut list, &older), "mais velho não troca");
        let mut newer = e.clone();
        newer.ts += 1000;
        newer.viewers = 5;
        assert!(merge_public(&mut list, &newer));
        assert_eq!(list[0].entry.viewers, 5);
        let mut gone = newer.clone();
        gone.ts += 1000;
        gone.gone = true;
        assert!(merge_public(&mut list, &gone), "encerrou sai da lista");
        assert!(list.is_empty());
        // A fofoca só aceita entrada assinada pelo host do código.
        let sealed = seal_entry(&keys, &newer).unwrap();
        assert_eq!(gossip_check(&sealed), Some((code.clone(), newer.ts)));
        let (_, other) = host();
        assert_eq!(gossip_check(&seal_entry(&other, &newer).unwrap()), None, "assinada por outra chave");
        let mut stale = newer.clone();
        stale.ts = now_ms() - PUBLIC_TTL_MS - 1000;
        assert_eq!(gossip_check(&seal_entry(&keys, &stale).unwrap()), None, "velha demais");
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
    fn password_changes_the_keys() {
        let (code, host) = Keys::new_host(Some("pizza")).unwrap();
        assert_eq!(check_code(&code), Ok(true), "código marca que tem senha");
        assert_eq!(check_code(&Keys::new_host(None).unwrap().0), Ok(false));
        let right = Keys::from_code(&code, Some("pizza")).unwrap();
        assert_eq!(host.viewer.to_bytes(), right.viewer.to_bytes());
        let msg = r#"{"t":"offer"}"#;
        assert_eq!(right.open(&host.seal(msg).unwrap()).as_deref(), Some(msg));
        // Senha errada: chaves diferentes, não lê nem escreve onde o host olha.
        let wrong = Keys::from_code(&code, Some("pizzA")).unwrap();
        assert_ne!(host.viewer.to_bytes(), wrong.viewer.to_bytes());
        assert!(wrong.open(&host.seal(msg).unwrap()).is_none());
        // Sem senha: recusa antes de tentar.
        assert!(Keys::from_code(&code, None).err().unwrap().contains("senha"));
        // A mesma chave pública sem a senha dá chaves diferentes (o segredo entra na conta).
        let pk = URL_SAFE_NO_PAD.encode(host.host_pk);
        let open_keys = Keys::from_code(&format!("{CODE_PREFIX}{pk}"), None).unwrap();
        assert_ne!(open_keys.viewer.to_bytes(), host.viewer.to_bytes());
    }

    #[test]
    fn mural_padlock_must_match_the_code() {
        let (code, _) = Keys::new_host(Some("x")).unwrap();
        let mut e = entry(&code, now_ms(), false);
        assert!(!valid(&e), "transmissão com senha anunciada sem cadeado");
        e.locked = true;
        assert!(valid(&e));
        let (open, _) = Keys::new_host(None).unwrap();
        let mut f = entry(&open, now_ms(), false);
        f.locked = true;
        assert!(!valid(&f), "cadeado num código sem senha");
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
        let viewer_keys = Arc::new(Keys::from_code(&code, None).unwrap());
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
        let code = host_listen(&host, on_offer, None).unwrap();
        let t = Instant::now();
        let answer = join(prepare_join(&viewer, &code, None).unwrap(), "s1", r#"{"t":"offer","sid":"s1"}"#, 90, &AtomicBool::new(false), |_| {}).unwrap();
        println!("resposta em {:?}", t.elapsed());
        assert!(answer.contains("answer"));
        host_stop(&host);
        assert!(host_reply(&host, &code, 0, "{}").is_err(), "depois de parar, o código não responde mais");
    }

    /// Como na vida real: host e espectador em nós da DHT diferentes, e o espectador
    /// entrando várias vezes seguidas (sai e entra de novo). Mede quanto cada entrada leva.
    #[test]
    #[ignore]
    fn live_repeated_joins() {
        let host = Arc::new(RdvState::default());
        let viewer = RdvState::default();
        let _ = host.dht.set(Dht::client().unwrap());
        let _ = viewer.dht.set(Dht::client().unwrap());
        assert!(host.dht.get().unwrap().bootstrapped() && viewer.dht.get().unwrap().bootstrapped());
        let h = host.clone();
        let on_offer: OfferFn = Arc::new(move |code: &str, slot: u8, offer: String| {
            let sid = serde_json::from_str::<serde_json::Value>(&offer).unwrap()["sid"].as_str().unwrap().to_string();
            println!("  host viu {sid} no slot {slot}");
            let (h, code) = (h.clone(), code.to_string());
            thread::spawn(move || host_reply(&h, &code, slot, &serde_json::json!({ "t": "answer", "sid": sid }).to_string()).unwrap());
        });
        let code = host_listen(&host, on_offer, None).unwrap();
        let runs: usize = std::env::var("AJ_RUNS").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
        let mut fails = 0;
        for i in 0..runs {
            let sid = format!("s{i}");
            let t = Instant::now();
            let r = join(prepare_join(&viewer, &code, None).unwrap(), &sid, &serde_json::json!({ "t": "offer", "sid": sid }).to_string(), 90, &AtomicBool::new(false), |e| {
                println!("  {e:?} em {:?}", t.elapsed())
            });
            match r {
                Ok(_) => println!("entrada {i}: resposta em {:?}", t.elapsed()),
                Err(e) => {
                    fails += 1;
                    println!("entrada {i}: FALHOU em {:?}: {e}", t.elapsed())
                }
            }
        }
        host_stop(&host);
        assert_eq!(fails, 0);
    }

    /// Host ocupado: o primeiro pedido recebe "busy", o espectador repete sozinho e o
    /// pedido repetido recebe a resposta.
    #[test]
    #[ignore]
    fn live_busy_then_answer() {
        let host = Arc::new(RdvState::default());
        let viewer = RdvState::default();
        let _ = host.dht.set(Dht::client().unwrap());
        let _ = viewer.dht.set(Dht::client().unwrap());
        let h = host.clone();
        let seen = Arc::new(Mutex::new(0u32));
        let on_offer: OfferFn = Arc::new(move |code: &str, slot: u8, offer: String| {
            let v: serde_json::Value = serde_json::from_str(&offer).unwrap();
            let sid = v["sid"].as_str().unwrap().to_string();
            if v["t"] != "offer" {
                return;
            }
            let n = { let mut s = seen.lock().unwrap(); *s += 1; *s };
            let t = if n == 1 { "busy" } else { "answer" };
            println!("  host viu o pedido {n}: responde {t}");
            let (h, code) = (h.clone(), code.to_string());
            thread::spawn(move || host_reply(&h, &code, slot, &serde_json::json!({ "t": t, "sid": sid }).to_string()).unwrap());
        });
        let code = host_listen(&host, on_offer, None).unwrap();
        let events = Mutex::new(Vec::new());
        let t = Instant::now();
        let answer = join(prepare_join(&viewer, &code, None).unwrap(), "b1", r#"{"t":"offer","sid":"b1","sdp":"x"}"#, 90, &AtomicBool::new(false), |e| {
            println!("  {e:?} em {:?}", t.elapsed());
            events.lock().unwrap().push(e);
        })
        .unwrap();
        println!("resposta em {:?}", t.elapsed());
        assert!(answer.contains("answer"));
        assert_eq!(*events.lock().unwrap(), vec![JoinEvent::Published, JoinEvent::Busy]);
        host_stop(&host);
    }

    /// Com senha, pela DHT real: a senha certa recebe resposta; a errada, não.
    #[test]
    #[ignore]
    fn live_join_with_password() {
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
        let t = Instant::now();
        let code = host_listen(&host, on_offer, Some("pizza")).unwrap();
        println!("código com senha gerado em {:?}: {code}", t.elapsed());
        let stop = AtomicBool::new(false);
        let wrong = join(prepare_join(&viewer, &code, Some("errada")).unwrap(), "s1", r#"{"t":"offer","sid":"s1"}"#, 25, &stop, |_| {});
        assert!(wrong.is_err(), "senha errada não pode receber resposta");
        let t = Instant::now();
        let right = join(prepare_join(&viewer, &code, Some("pizza")).unwrap(), "s2", r#"{"t":"offer","sid":"s2"}"#, 90, &stop, |_| {}).unwrap();
        println!("resposta com a senha certa em {:?}", t.elapsed());
        assert!(right.contains("answer"));
        host_stop(&host);
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
