//! Canal de encontro por relays Nostr (docs/encontro-nostr.md): mais um caminho para o
//! pedido e a resposta, além da DHT e do encontro direto. Vale o que chegar primeiro.
//!
//! Os relays Nostr são centenas de servidores públicos e gratuitos, de gente diferente,
//! que repassam mensagens assinadas por WebSocket (porta 443, passa em quase qualquer
//! rede). Usamos eventos efêmeros (tipo 20000–29999): o relay entrega na hora a quem está
//! inscrito e não guarda. Nenhum servidor nosso, nenhuma conta: cada canal usa uma chave
//! descartável só para assinar os eventos (exigência dos relays).
//!
//! O conteúdo vai cifrado com a chave do código (quem chama este módulo cifra); os relays
//! só veem a marca do canal e bytes embaralhados.

use k256::schnorr::SigningKey;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Relays públicos usados (o primeiro que entregar vale).
pub const RELAYS: [&str; 4] = ["wss://relay.damus.io", "wss://nos.lol", "wss://relay.primal.net", "wss://offchain.pub"];
/// Tipo dos eventos (efêmero: o relay não guarda).
const KIND: u64 = 25_050;
const MAX_CONTENT: usize = 16 * 1024;

fn log(msg: impl AsRef<str>) {
    crate::connlog::log(format!("nostr: {}", msg.as_ref()));
}

/// Desligado com `AJ_NO_NOSTR`.
pub fn enabled() -> bool {
    std::env::var_os("AJ_NO_NOSTR").is_none()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Evento Nostr assinado (NIP-01).
fn event(key: &SigningKey, tag: &str, content: &str) -> Option<Value> {
    let pubkey = hex(&key.verifying_key().to_bytes());
    let created_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let tags = json!([["t", tag]]);
    let serial = serde_json::to_string(&json!([0, pubkey, created_at, KIND, tags, content])).ok()?;
    let id: [u8; 32] = Sha256::digest(serial.as_bytes()).into();
    let mut aux = [0u8; 32];
    getrandom::fill(&mut aux).ok()?;
    let sig = key.sign_raw(&id, &aux).ok()?;
    Some(json!({ "id": hex(&id), "pubkey": pubkey, "created_at": created_at, "kind": KIND, "tags": tags, "content": content, "sig": hex(&sig.to_bytes()) }))
}

fn random_key() -> Option<SigningKey> {
    loop {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).ok()?;
        if let Ok(k) = SigningKey::from_slice(&b) {
            return Some(k);
        }
    }
}

struct Shared {
    stop: AtomicBool,
    /// Eventos a publicar (todos os relays mandam todos, na ordem).
    outbox: Mutex<Vec<String>>,
    /// Eventos já entregues (o mesmo evento chega por vários relays).
    seen: Mutex<HashSet<String>>,
}

/// Um canal: escuta uma marca e publica em outras. Fecha ao ser solto.
pub struct Channel {
    shared: Arc<Shared>,
    key: SigningKey,
}

impl Channel {
    /// Abre o canal: cada mensagem com a marca `listen` vai para `on_msg` (o conteúdo).
    pub fn open(listen: &str, on_msg: impl Fn(String) + Send + Sync + 'static) -> Option<Channel> {
        if !enabled() {
            return None;
        }
        let key = random_key()?;
        let shared = Arc::new(Shared { stop: AtomicBool::new(false), outbox: Mutex::new(Vec::new()), seen: Mutex::new(HashSet::new()) });
        let on_msg: Arc<dyn Fn(String) + Send + Sync> = Arc::new(on_msg);
        for url in RELAYS {
            let (shared, on_msg, listen) = (shared.clone(), on_msg.clone(), listen.to_string());
            thread::Builder::new().name("aj-nostr".into()).spawn(move || relay_loop(url, &listen, &shared, &on_msg)).ok()?;
        }
        Some(Channel { shared, key })
    }

    /// Publica `content` com a marca `tag` em todos os relays.
    pub fn publish(&self, tag: &str, content: &str) {
        if content.len() > MAX_CONTENT {
            return;
        }
        if let Some(ev) = event(&self.key, tag, content) {
            self.shared.outbox.lock().unwrap().push(json!(["EVENT", ev]).to_string());
        }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
    }
}

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

fn set_timeout(ws: &mut Ws, t: Duration) {
    let _ = match ws.get_mut() {
        MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(t)),
        MaybeTlsStream::Rustls(s) => s.sock.set_read_timeout(Some(t)),
        _ => Ok(()),
    };
}

/// Um relay: conecta, se inscreve na marca, publica o que entrar na fila e entrega o que
/// chegar. Reconecta (com espera crescente) se cair.
fn relay_loop(url: &str, listen: &str, shared: &Shared, on_msg: &Arc<dyn Fn(String) + Send + Sync>) {
    let mut backoff = Duration::from_secs(2);
    let mut logged = false;
    while !shared.stop.load(Ordering::SeqCst) {
        let t = Instant::now();
        let mut ws = match tungstenite::connect(url) {
            Ok((ws, _)) => ws,
            Err(_) => {
                sleep_unless(shared, backoff);
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        if !logged {
            log(format!("{} conectado em {} ms", url.trim_start_matches("wss://"), t.elapsed().as_millis()));
            logged = true;
        }
        backoff = Duration::from_secs(2);
        set_timeout(&mut ws, Duration::from_millis(200));
        let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs().saturating_sub(60)).unwrap_or(0);
        let sub = "aj";
        if ws.send(Message::text(json!(["REQ", sub, { "kinds": [KIND], "#t": [listen], "since": since }]).to_string())).is_err() {
            continue;
        }
        let mut sent = 0usize;
        loop {
            if shared.stop.load(Ordering::SeqCst) {
                let _ = ws.send(Message::text(json!(["CLOSE", sub]).to_string()));
                let _ = ws.close(None);
                return;
            }
            let pending: Vec<String> = shared.outbox.lock().unwrap()[sent..].to_vec();
            let mut failed = false;
            for p in pending {
                if ws.send(Message::text(p)).is_err() {
                    failed = true;
                    break;
                }
                sent += 1;
            }
            if failed {
                break;
            }
            match ws.read() {
                Ok(Message::Text(t)) => deliver(t.as_str(), listen, shared, on_msg),
                Ok(Message::Ping(p)) => {
                    let _ = ws.send(Message::Pong(p));
                }
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => break,
            }
        }
        sleep_unless(shared, backoff);
    }
}

fn sleep_unless(shared: &Shared, d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until && !shared.stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(100));
    }
}

/// `["EVENT", sub, evento]` com a nossa marca e ainda não visto: entrega o conteúdo.
fn deliver(text: &str, listen: &str, shared: &Shared, on_msg: &Arc<dyn Fn(String) + Send + Sync>) {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return };
    if v[0] != "EVENT" {
        return;
    }
    let ev = &v[2];
    let tagged = ev["tags"].as_array().is_some_and(|t| t.iter().any(|t| t[0] == "t" && t[1] == listen));
    let (Some(id), Some(content)) = (ev["id"].as_str(), ev["content"].as_str()) else { return };
    if !tagged || ev["kind"] != KIND || content.len() > MAX_CONTENT {
        return;
    }
    if !shared.seen.lock().unwrap().insert(id.to_string()) {
        return;
    }
    on_msg(content.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O evento segue a NIP-01: id = sha256 da serialização; a assinatura confere com a
    /// chave pública (x-only) sobre o id.
    #[test]
    fn events_are_signed_per_nip01() {
        let key = random_key().unwrap();
        let ev = event(&key, "aj-teste", "conteúdo").unwrap();
        let serial = serde_json::to_string(&json!([0, ev["pubkey"], ev["created_at"], ev["kind"], ev["tags"], ev["content"]])).unwrap();
        let id: [u8; 32] = Sha256::digest(serial.as_bytes()).into();
        assert_eq!(ev["id"].as_str().unwrap(), hex(&id));
        let sig_hex = ev["sig"].as_str().unwrap();
        let sig_bytes: Vec<u8> = (0..sig_hex.len()).step_by(2).map(|i| u8::from_str_radix(&sig_hex[i..i + 2], 16).unwrap()).collect();
        let sig = k256::schnorr::Signature::try_from(sig_bytes.as_slice()).unwrap();
        key.verifying_key().verify_raw(&id, &sig).unwrap();
        assert_eq!(ev["pubkey"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn delivers_only_our_tag_once() {
        let shared = Shared { stop: AtomicBool::new(false), outbox: Mutex::new(Vec::new()), seen: Mutex::new(HashSet::new()) };
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let on: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |c| g.lock().unwrap().push(c));
        let key = random_key().unwrap();
        let ours = json!(["EVENT", "aj", event(&key, "minha", "a").unwrap()]).to_string();
        let other = json!(["EVENT", "aj", event(&key, "outra", "b").unwrap()]).to_string();
        deliver(&ours, "minha", &shared, &on);
        deliver(&ours, "minha", &shared, &on); // o mesmo evento por outro relay
        deliver(&other, "minha", &shared, &on);
        deliver(r#"["NOTICE","oi"]"#, "minha", &shared, &on);
        assert_eq!(*got.lock().unwrap(), vec!["a".to_string()]);
    }

    /// Pelos relays de verdade: um canal publica, o outro recebe.
    /// `cargo test nostr_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn nostr_live_roundtrip() {
        let mut b = [0u8; 8];
        getrandom::fill(&mut b).unwrap();
        let tag = format!("aj-teste-{}", hex(&b));
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        let _rx_chan = Channel::open(&tag, move |c| {
            let _ = tx.lock().unwrap().send(c);
        })
        .unwrap();
        thread::sleep(Duration::from_secs(3)); // inscrições feitas
        let tx_chan = Channel::open("aj-teste-ninguem", |_| {}).unwrap();
        let t = Instant::now();
        tx_chan.publish(&tag, "olá pelo nostr");
        let got = rx.recv_timeout(Duration::from_secs(15)).expect("nada chegou pelos relays");
        println!("chegou em {:?}", t.elapsed());
        assert_eq!(got, "olá pelo nostr");
    }
}
