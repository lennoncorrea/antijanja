//! Atualização automática pelas releases do GitHub.
//!
//! Lê o `latest.json` da última release, baixa o instalador, confere a assinatura
//! minisign com a chave pública embutida e roda o instalador em modo passivo
//! (`/P /UPDATE /R`: instala sem perguntar e reabre o app). Mesmo formato de antes,
//! então versões antigas (Tauri) também atualizam para esta.

use base64::{engine::general_purpose::STANDARD, Engine};
use std::io::Read;

const ENDPOINT: &str = "https://github.com/lennoncorrea/antijanja/releases/latest/download/latest.json";
/// Chave pública (minisign, em base64) que assina os instaladores.
const PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEFBMkFDNDY3MTI1MkI5OEUKUldTT3VWSVNaOFFxcWoxd01MNC9nSWJDdVFpUmZ2VWpTM0lMOGJ0TnVwc1hUUlNZcjg0SHNDcVgK";
const MAX_INSTALLER: u64 = 300 * 1024 * 1024;

pub struct Update {
    pub version: String,
    url: String,
    signature: String,
}

fn parse_version(v: &str) -> Vec<u64> {
    v.trim_start_matches('v').split(['.', '-', '+']).take(3).map(|p| p.parse().unwrap_or(0)).collect()
}

fn newer(remote: &str, local: &str) -> bool {
    parse_version(remote) > parse_version(local)
}

/// Versão real da release. Desde a 0.3.2 as versões são 0.x: ela vem em `app_version`.
/// O campo `version` fica fixo e alto (3.999.0) só para os apps antigos (2.x e 3.x),
/// que só leem ele e só atualizam para uma versão "maior" que a deles.
fn release_version(json: &serde_json::Value) -> Option<String> {
    json["app_version"].as_str().or(json["version"].as_str()).map(str::to_string)
}

/// A versão também vira nome de arquivo: só dígitos, pontos e um `v` opcional.
fn safe_version(v: &str) -> bool {
    let v = v.strip_prefix('v').unwrap_or(v);
    !v.is_empty() && v.len() <= 32 && v.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build()
        .into()
}

/// Há uma versão mais nova que esta?
pub fn check() -> Result<Option<Update>, String> {
    let mut resp = agent().get(ENDPOINT).call().map_err(|e| e.to_string())?;
    let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let version = release_version(&json).ok_or("latest.json sem versão")?;
    if !safe_version(&version) {
        return Err("latest.json com versão inválida".into());
    }
    if !newer(&version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let platform = &json["platforms"]["windows-x86_64"];
    let url = platform["url"].as_str().ok_or("latest.json sem url")?.to_string();
    let signature = platform["signature"].as_str().ok_or("latest.json sem assinatura")?.to_string();
    Ok(Some(Update { version, url, signature }))
}

fn verify(data: &[u8], signature_b64: &str) -> Result<(), String> {
    let decode = |b64: &str| -> Result<String, String> {
        let bytes = STANDARD.decode(b64.trim()).map_err(|e| e.to_string())?;
        String::from_utf8(bytes).map_err(|e| e.to_string())
    };
    let pk = minisign_verify::PublicKey::decode(&decode(PUBKEY)?).map_err(|e| e.to_string())?;
    let sig = minisign_verify::Signature::decode(&decode(signature_b64)?).map_err(|e| e.to_string())?;
    pk.verify(data, &sig, false).map_err(|_| "assinatura do instalador inválida".to_string())
}

/// Baixa, confere e inicia o instalador. `progress(baixado, total)`.
/// Se der certo, o chamador deve encerrar o app logo em seguida.
pub fn download_and_install(update: &Update, progress: impl Fn(u64, Option<u64>)) -> Result<(), String> {
    let mut resp = agent().get(&update.url).call().map_err(|e| e.to_string())?;
    let total = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    let mut reader = resp.body_mut().with_config().limit(MAX_INSTALLER).reader();
    let mut data = Vec::with_capacity(total.unwrap_or(8 << 20) as usize);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        progress(data.len() as u64, total);
    }
    verify(&data, &update.signature)?;

    let path = std::env::temp_dir().join(format!("AntiJanja_{}_setup.exe", update.version));
    std::fs::write(&path, &data).map_err(|e| e.to_string())?;
    std::process::Command::new(&path)
        .args(["/P", "/UPDATE", "/R"])
        .spawn()
        .map_err(|e| format!("não deu para abrir o instalador: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("3.0.0", "2.5.0"));
        assert!(newer("2.10.0", "2.9.9"));
        assert!(!newer("2.5.0", "2.5.0"));
        assert!(!newer("v2.4.9", "2.5.0"));
        assert!(newer("0.3.3", "0.3.2"));
    }

    #[test]
    fn reads_real_version_and_keeps_old_apps_updating() {
        let json = serde_json::json!({ "version": "3.999.0", "app_version": "0.3.2" });
        assert_eq!(release_version(&json).as_deref(), Some("0.3.2"));
        assert!(newer(json["version"].as_str().unwrap(), "3.1.0"), "apps 3.x precisam ver uma versão maior");
        assert_eq!(release_version(&serde_json::json!({ "version": "3.1.0" })).as_deref(), Some("3.1.0"));
    }

    #[test]
    fn rejects_unsafe_versions() {
        assert!(safe_version("0.3.2") && safe_version("v3.1.0"));
        assert!(!safe_version("9.9.9/../../x"));
        assert!(!safe_version("1.0.0-beta"));
        assert!(!safe_version(""));
    }

    /// Confere o instalador gerado por `scripts/release.ps1` contra a chave embutida:
    /// `cargo test -- --ignored verifies_release_signature`
    /// (`AJ_BUNDLE_DIR=<pasta>` confere outra pasta, ex.: uma release baixada do GitHub).
    #[test]
    #[ignore]
    fn verifies_release_signature() {
        let dir = std::env::var_os("AJ_BUNDLE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/release/bundle"));
        let latest: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("latest.json")).unwrap()).unwrap();
        let version = release_version(&latest).unwrap();
        let sig = latest["platforms"]["windows-x86_64"]["signature"].as_str().unwrap();
        let data = std::fs::read(dir.join(format!("AntiJanja_{version}_x64-setup.exe"))).unwrap();
        verify(&data, sig).expect("assinatura deve bater com a chave embutida");
        let mut tampered = data.clone();
        tampered[1000] ^= 1;
        assert!(verify(&tampered, sig).is_err(), "instalador alterado deve ser recusado");
    }

    #[test]
    fn public_key_decodes() {
        let text = String::from_utf8(STANDARD.decode(PUBKEY).unwrap()).unwrap();
        assert!(minisign_verify::PublicKey::decode(&text).is_ok());
    }
}
