//! Registro das etapas de conexão em `%APPDATA%\AntiJanja\conexao.log`, para entender
//! depois por que uma entrada travou (o encontro pela DHT não tem mensagem de erro para
//! a maioria dos problemas: o sintoma é só "o host não respondeu").
//!
//! Uma linha por evento, com data e hora. O arquivo é recomeçado quando passa de
//! `MAX_BYTES` (a versão anterior fica em `conexao.old.log`). Não grava códigos
//! inteiros, senhas nem endereços: só o começo do código e do id da tentativa.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

const MAX_BYTES: u64 = 2 * 1024 * 1024;

static LOCK: Mutex<()> = Mutex::new(());

fn path() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("APPDATA")?).join("AntiJanja");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("conexao.log"))
}

/// Começo de um código ou id, para reconhecer nas linhas sem expor o resto.
pub fn short(s: &str) -> String {
    let body = s.strip_prefix(crate::rendezvous::CODE_PREFIX).unwrap_or(s);
    body.chars().take(6).collect()
}

/// Grava uma linha (e repete no terminal com `AJ_TRACE`). Falhas ao gravar são ignoradas.
pub fn log(msg: impl AsRef<str>) {
    let msg = msg.as_ref();
    let line = format!("{} {msg}\n", stamp());
    if std::env::var_os("AJ_TRACE").is_some() {
        eprint!("[conexão] {line}");
    }
    let Some(p) = path() else { return };
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(&p, p.with_file_name("conexao.old.log"));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Data e hora local, com milissegundos.
fn stamp() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_hides_most_of_the_code() {
        assert_eq!(short("AJ3.abcdefghijklmnop"), "abcdef");
        assert_eq!(short("1234567890abcdef"), "123456");
        assert_eq!(short("ab"), "ab");
    }
}
