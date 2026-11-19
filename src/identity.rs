//! A conta do usuário (docs/amizades.md): chave mestra + uma chave por aparelho.
//!
//! - A **chave mestra** (Ed25519) é a identidade; a parte pública vira o código de amigo
//!   (`AJF.…`). Ela sai da frase de recuperação (24 palavras) e só fica guardada nos
//!   aparelhos "principais": onde a conta foi criada ou restaurada pela frase. Ela assina
//!   os certificados dos aparelhos e o cartão da conta (com os aparelhos desvinculados).
//! - A **chave de caixa** (X25519) cifra o que vai para os amigos: com a de um amigo, dá o
//!   segredo do par (Diffie-Hellman). Também sai da frase, e todos os aparelhos têm.
//! - Cada **aparelho** sorteia a própria chave (Ed25519) e recebe um certificado assinado
//!   pela mestra ("este aparelho fala pela conta"). Tudo que um aparelho manda aos amigos
//!   vai assinado por ele, com o certificado junto: desvinculado, os amigos recusam.
//!
//! Guardado em `%APPDATA%\AntiJanja\identity.json`.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use bip39::{Language, Mnemonic};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use x25519_dalek::{PublicKey as BoxPublic, StaticSecret};

pub const FRIEND_PREFIX: &str = "AJF.";
const CERT_CONTEXT: &[u8] = b"antijanja device cert v1:";
const DEVICE_SIG_CONTEXT: &[u8] = b"antijanja device sig v1:";
const MAX_DEVICE_NAME: usize = 32;

/// Certificado de um aparelho: a mestra assina (chave do aparelho, criação, nome).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct DeviceCert {
    /// Chave pública do aparelho (base64).
    pub dev: String,
    pub name: String,
    /// Criação (ms desde 1970).
    pub at: i64,
    /// Assinatura da mestra (base64).
    pub sig: String,
}

fn cert_message(dev: &[u8; 32], at: i64, name: &str) -> Vec<u8> {
    [CERT_CONTEXT, dev, &at.to_be_bytes(), name.as_bytes()].concat()
}

impl DeviceCert {
    fn issue(master: &SigningKey, dev: &[u8; 32], name: &str, at: i64) -> Self {
        let sig = master.sign(&cert_message(dev, at, name));
        DeviceCert { dev: B64.encode(dev), name: name.to_string(), at, sig: B64.encode(sig.to_bytes()) }
    }

    pub fn device_key(&self) -> Option<VerifyingKey> {
        VerifyingKey::from_bytes(&decode32(&self.dev)?).ok()
    }

    /// Assinado por esta mestra?
    pub fn valid_for(&self, master: &VerifyingKey) -> bool {
        let (Some(dev), Some(sig)) = (decode32(&self.dev), decode_sig(&self.sig)) else { return false };
        self.name.chars().count() <= MAX_DEVICE_NAME && master.verify_strict(&cert_message(&dev, self.at, &self.name), &sig).is_ok()
    }
}

/// Uma mensagem assinada por um aparelho da conta (com o certificado dele).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DeviceSig {
    pub cert: DeviceCert,
    pub sig: String,
}

impl DeviceSig {
    /// Confere: certificado da mestra `master`, aparelho não desvinculado e assinatura de
    /// `ctx || msg` pela chave do aparelho.
    pub fn verify(&self, master: &VerifyingKey, revoked: &[String], ctx: &[u8], msg: &[u8]) -> bool {
        if !self.cert.valid_for(master) || revoked.contains(&self.cert.dev) {
            return false;
        }
        let (Some(dev), Some(sig)) = (self.cert.device_key(), decode_sig(&self.sig)) else { return false };
        dev.verify_strict(&[DEVICE_SIG_CONTEXT, ctx, msg].concat(), &sig).is_ok()
    }
}

/// O que fica no disco.
#[derive(Serialize, Deserialize, Clone, Default)]
struct Stored {
    /// Entropia da frase (só nos aparelhos principais).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entropy: Option<String>,
    master_pub: String,
    box_secret: String,
    device_secret: String,
    cert: Option<DeviceCert>,
}

pub struct Identity {
    master_pub: VerifyingKey,
    /// Só nos aparelhos principais.
    master: Option<SigningKey>,
    entropy: Option<[u8; 32]>,
    box_secret: StaticSecret,
    device: SigningKey,
    cert: DeviceCert,
}

fn decode32(s: &str) -> Option<[u8; 32]> {
    B64.decode(s.trim()).ok()?.try_into().ok()
}

fn decode_sig(s: &str) -> Option<Signature> {
    Signature::from_slice(&B64.decode(s).ok()?).ok()
}

fn random32() -> [u8; 32] {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("gerador aleatório do sistema");
    b
}

/// Chaves que saem da frase: (mestra, caixa).
fn keys_from_entropy(entropy: &[u8; 32]) -> (SigningKey, StaticSecret) {
    let master = SigningKey::from_bytes(&blake3::derive_key("antijanja master key v1", entropy));
    let boxk = StaticSecret::from(blake3::derive_key("antijanja box key v1", entropy));
    (master, boxk)
}

pub fn default_device_name() -> String {
    let n = std::env::var("COMPUTERNAME").unwrap_or_default();
    let n = n.trim();
    if n.is_empty() { "Este PC".into() } else { n.chars().take(MAX_DEVICE_NAME).collect() }
}

/// Lê o código de amigo (`AJF.…`, com espaços em volta tolerados).
pub fn parse_friend_code(code: &str) -> Result<VerifyingKey, String> {
    let body = code.trim().strip_prefix(FRIEND_PREFIX).ok_or("Código de amigo inválido.")?;
    let raw = decode32(body).ok_or("Código de amigo inválido.")?;
    VerifyingKey::from_bytes(&raw).map_err(|_| "Código de amigo inválido.".to_string())
}

pub fn friend_code_of(master: &VerifyingKey) -> String {
    format!("{FRIEND_PREFIX}{}", B64.encode(master.as_bytes()))
}

/// Impressão curta da chave (para conferir de olho e diferenciar nomes repetidos):
/// 4 figuras (forma 0–3, cor 0–5) e um texto como "K7Q·F2M".
pub fn fingerprint(master: &[u8; 32]) -> ([(u8, u8); 4], String) {
    const ALPHA: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let h = blake3::derive_key("antijanja fingerprint v1", master);
    let shapes = [0, 1, 2, 3].map(|i| (h[i] % 4, h[4 + i] % 6));
    let t: String = h[8..14].iter().map(|b| ALPHA[(*b as usize) % ALPHA.len()] as char).collect();
    (shapes, format!("{}·{}", &t[..3], &t[3..]))
}

impl Identity {
    /// Conta nova: sorteia a frase.
    pub fn create(device_name: &str) -> Self {
        Self::from_entropy(random32(), device_name)
    }

    /// Restaura pela frase (este aparelho vira principal).
    pub fn restore(phrase: &str, device_name: &str) -> Result<Self, String> {
        let normalized = phrase.split_whitespace().map(|w| w.to_lowercase()).collect::<Vec<_>>().join(" ");
        let m = Mnemonic::parse_in(Language::Portuguese, &normalized).map_err(|_| "Frase inválida: confira as 24 palavras.".to_string())?;
        let entropy: [u8; 32] = m.to_entropy().try_into().map_err(|_| "A frase precisa ter 24 palavras.".to_string())?;
        Ok(Self::from_entropy(entropy, device_name))
    }

    fn from_entropy(entropy: [u8; 32], device_name: &str) -> Self {
        let (master, box_secret) = keys_from_entropy(&entropy);
        let device = SigningKey::from_bytes(&random32());
        let name: String = device_name.chars().take(MAX_DEVICE_NAME).collect();
        let cert = DeviceCert::issue(&master, device.verifying_key().as_bytes(), &name, crate::rendezvous::now_ms());
        Identity { master_pub: master.verifying_key(), master: Some(master), entropy: Some(entropy), box_secret, device, cert }
    }

    /// Aparelho vinculado: recebe do principal a chave pública da conta, a chave de caixa
    /// e o certificado da sua chave de aparelho (sorteada aqui antes do pedido).
    pub fn linked(master_pub: VerifyingKey, box_secret: [u8; 32], device: SigningKey, cert: DeviceCert) -> Result<Self, String> {
        if !cert.valid_for(&master_pub) || cert.device_key() != Some(device.verifying_key()) {
            return Err("Certificado do aparelho inválido.".into());
        }
        Ok(Identity { master_pub, master: None, entropy: None, box_secret: StaticSecret::from(box_secret), device, cert })
    }

    pub fn master_pub(&self) -> VerifyingKey {
        self.master_pub
    }

    pub fn friend_code(&self) -> String {
        friend_code_of(&self.master_pub)
    }

    pub fn fingerprint(&self) -> ([(u8, u8); 4], String) {
        fingerprint(self.master_pub.as_bytes())
    }

    /// Aparelho principal (tem a chave mestra: vincula e desvincula aparelhos).
    pub fn is_primary(&self) -> bool {
        self.master.is_some()
    }

    pub fn master(&self) -> Option<&SigningKey> {
        self.master.as_ref()
    }

    /// As 24 palavras (só no aparelho principal).
    pub fn phrase(&self) -> Option<String> {
        let e = self.entropy?;
        Some(Mnemonic::from_entropy_in(Language::Portuguese, &e).ok()?.to_string())
    }

    pub fn box_public(&self) -> [u8; 32] {
        BoxPublic::from(&self.box_secret).to_bytes()
    }

    pub fn box_secret_bytes(&self) -> [u8; 32] {
        self.box_secret.to_bytes()
    }

    /// Segredo Diffie-Hellman com a chave de caixa de outra conta.
    pub fn dh(&self, their_box: &[u8; 32]) -> [u8; 32] {
        self.box_secret.diffie_hellman(&BoxPublic::from(*their_box)).to_bytes()
    }

    pub fn device_key(&self) -> &SigningKey {
        &self.device
    }

    pub fn cert(&self) -> &DeviceCert {
        &self.cert
    }

    /// Assina `ctx || msg` como este aparelho.
    pub fn sign(&self, ctx: &[u8], msg: &[u8]) -> DeviceSig {
        let sig = self.device.sign(&[DEVICE_SIG_CONTEXT, ctx, msg].concat());
        DeviceSig { cert: self.cert.clone(), sig: B64.encode(sig.to_bytes()) }
    }

    /// Certificado para outro aparelho (só no principal).
    pub fn issue_cert(&self, dev: &VerifyingKey, name: &str) -> Option<DeviceCert> {
        let name: String = name.trim().chars().take(MAX_DEVICE_NAME).collect();
        Some(DeviceCert::issue(self.master.as_ref()?, dev.as_bytes(), &name, crate::rendezvous::now_ms()))
    }

    /// Renomeia este aparelho (no principal, com certificado novo).
    pub fn rename_device(&mut self, name: &str) -> bool {
        match self.issue_cert(&self.device.verifying_key(), name) {
            Some(c) => {
                self.cert = c;
                true
            }
            None => false,
        }
    }

    fn to_stored(&self) -> Stored {
        Stored {
            entropy: self.entropy.map(|e| B64.encode(e)),
            master_pub: B64.encode(self.master_pub.as_bytes()),
            box_secret: B64.encode(self.box_secret.to_bytes()),
            device_secret: B64.encode(self.device.to_bytes()),
            cert: Some(self.cert.clone()),
        }
    }

    fn from_stored(s: Stored) -> Option<Self> {
        let device = SigningKey::from_bytes(&decode32(&s.device_secret)?);
        let cert = s.cert?;
        match s.entropy.as_deref().and_then(decode32) {
            Some(entropy) => {
                let (master, box_secret) = keys_from_entropy(&entropy);
                (B64.encode(master.verifying_key().as_bytes()) == s.master_pub && cert.valid_for(&master.verifying_key())).then(|| Identity {
                    master_pub: master.verifying_key(),
                    master: Some(master),
                    entropy: Some(entropy),
                    box_secret,
                    device,
                    cert,
                })
            }
            None => {
                let master_pub = VerifyingKey::from_bytes(&decode32(&s.master_pub)?).ok()?;
                Identity::linked(master_pub, decode32(&s.box_secret)?, device, cert).ok()
            }
        }
    }

    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(&self.to_stored()).unwrap_or_default()
    }

    pub fn from_json(b: &[u8]) -> Option<Self> {
        Self::from_stored(serde_json::from_slice(b).ok()?)
    }
}

fn path() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("APPDATA")?).join("AntiJanja");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("identity.json"))
}

/// A conta deste PC, se já existir.
pub fn load() -> Option<Identity> {
    Identity::from_json(&std::fs::read(path()?).ok()?)
}

pub fn save(id: &Identity) -> Result<(), String> {
    let p = path().ok_or("Pasta de configurações indisponível.")?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, id.to_json()).map_err(|e| e.to_string())?;
    std::fs::rename(tmp, p).map_err(|e| e.to_string())
}

/// A conta deste PC; na primeira vez, cria uma.
pub fn load_or_create() -> Identity {
    if let Some(id) = load() {
        return id;
    }
    let id = Identity::create(&default_device_name());
    if let Err(e) = save(&id) {
        crate::connlog::log(format!("conta: não gravou: {e}"));
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrase_restores_same_account_and_box_key() {
        let a = Identity::create("PC");
        let phrase = a.phrase().unwrap();
        assert_eq!(phrase.split_whitespace().count(), 24);
        // Maiúsculas e espaços a mais não atrapalham.
        let b = Identity::restore(&format!("  {}  ", phrase.to_uppercase().replace(' ', "   ")), "Notebook").unwrap();
        assert_eq!(a.master_pub(), b.master_pub());
        assert_eq!(a.box_public(), b.box_public());
        assert_ne!(a.device_key().to_bytes(), b.device_key().to_bytes(), "cada aparelho tem a sua chave");
        assert!(b.is_primary());
        assert!(Identity::restore("palavra errada", "x").is_err());
    }

    #[test]
    fn friend_code_roundtrip_and_fingerprint() {
        let a = Identity::create("PC");
        let code = a.friend_code();
        assert!(code.starts_with(FRIEND_PREFIX));
        assert_eq!(parse_friend_code(&format!(" {code}\n")).unwrap(), a.master_pub());
        assert!(parse_friend_code("AJ3.abc").is_err());
        let (shapes, text) = a.fingerprint();
        assert_eq!(text.chars().count(), 7);
        assert!(shapes.iter().all(|(s, c)| *s < 4 && *c < 6));
        assert_eq!(a.fingerprint(), fingerprint(a.master_pub().as_bytes()));
    }

    #[test]
    fn device_signatures_check_cert_and_revocation() {
        let a = Identity::create("PC");
        let msg = b"oi";
        let s = a.sign(b"ctx", msg);
        let m = a.master_pub();
        assert!(s.verify(&m, &[], b"ctx", msg));
        assert!(!s.verify(&m, &[], b"outro", msg), "contexto diferente");
        assert!(!s.verify(&m, &[], b"ctx", b"tchau"), "mensagem trocada");
        assert!(!s.verify(&m, &[s.cert.dev.clone()], b"ctx", msg), "aparelho desvinculado");
        let other = Identity::create("PC");
        assert!(!s.verify(&other.master_pub(), &[], b"ctx", msg), "certificado de outra conta");
    }

    #[test]
    fn linked_device_gets_cert_but_no_master() {
        let a = Identity::create("PC");
        let dev = SigningKey::from_bytes(&random32());
        let cert = a.issue_cert(&dev.verifying_key(), "Notebook").unwrap();
        let b = Identity::linked(a.master_pub(), a.box_secret_bytes(), dev, cert.clone()).unwrap();
        assert!(!b.is_primary() && b.phrase().is_none());
        assert!(b.issue_cert(&a.device_key().verifying_key(), "x").is_none());
        assert_eq!(a.dh(&Identity::create("x").box_public()).len(), 32);
        // Certificado de outro aparelho não serve.
        let wrong = SigningKey::from_bytes(&random32());
        assert!(Identity::linked(a.master_pub(), a.box_secret_bytes(), wrong, cert).is_err());
        // Gravar e ler de volta.
        let b2 = Identity::from_json(&b.to_json()).unwrap();
        assert_eq!(b2.master_pub(), a.master_pub());
        assert!(!b2.is_primary());
        let a2 = Identity::from_json(&a.to_json()).unwrap();
        assert!(a2.is_primary());
        assert_eq!(a2.phrase(), a.phrase());
    }

    #[test]
    fn pair_secret_matches_both_sides() {
        let (a, b) = (Identity::create("a"), Identity::create("b"));
        assert_eq!(a.dh(&b.box_public()), b.dh(&a.box_public()));
    }
}
