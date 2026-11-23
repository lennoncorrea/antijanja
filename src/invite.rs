//! Link de convite: `antijanja://entrar/<código>`. Clicar no link abre o AntiJanja já
//! entrando na transmissão (ou adicionando o amigo, com um código de amigo).
//!
//! - O próprio app registra o `antijanja://` no Windows (em `HKCU`, sem pedir permissão)
//!   ao abrir. Só a versão instalada: a de desenvolvimento não toma o link para si.
//! - O Windows abre um app novo a cada clique. Se já houver um AntiJanja aberto, o novo só
//!   passa o código para ele (por uma porta local, só de 127.0.0.1) e fecha.
//!
//! O link leva só o código (o mesmo `AJ3.` de sempre), nada de endereço: mandar o link é
//! igual a mandar o código.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

pub const SCHEME: &str = "antijanja";
const LINK_PREFIX: &str = "antijanja://entrar/";
/// Maior código aceito por um link (os códigos têm ~50 caracteres).
const MAX_CODE: usize = 120;

/// O link de convite de um código.
pub fn link_for(code: &str) -> String {
    format!("{LINK_PREFIX}{code}")
}

/// O código dentro de um link (ou de um argumento da linha de comando que seja um link).
/// Aceita `antijanja://entrar/X`, `antijanja://X` e `antijanja:X`, com ou sem barra no fim.
pub fn code_from_link(arg: &str) -> Option<String> {
    let arg = arg.trim().trim_matches('"');
    let (scheme, rest) = arg.split_once(':')?;
    if !scheme.eq_ignore_ascii_case(SCHEME) {
        return None;
    }
    let rest = rest.trim_start_matches('/');
    let rest = rest.strip_prefix("entrar/").unwrap_or(rest);
    let code: String = percent_decode(rest.trim_end_matches('/')).chars().filter(|c| !c.is_whitespace()).collect();
    let ok = !code.is_empty() && code.len() <= MAX_CODE && code.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    ok.then_some(code)
}

/// `%2E` e afins (alguns apps de mensagem trocam o ponto do código por isso).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// O código do link com que o app foi aberto, se houver.
pub fn code_from_args() -> Option<String> {
    std::env::args().skip(1).find_map(|a| code_from_link(&a))
}

fn instance_file() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("APPDATA")?).join("AntiJanja");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("instancia.txt"))
}

/// Passa o código para um AntiJanja já aberto. `true` se ele recebeu (aí este app fecha).
pub fn forward(code: &str) -> bool {
    instance_file().is_some_and(|p| forward_via(&p, code))
}

fn forward_via(file: &std::path::Path, code: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(file) else { return false };
    let mut it = text.split_whitespace();
    let (Some(port), Some(pid)) = (it.next().and_then(|p| p.parse::<u16>().ok()), it.next().and_then(|p| p.parse::<u32>().ok())) else { return false };
    let Ok(mut s) = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), Duration::from_millis(500)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    // Deixa o app aberto vir para a frente (quem clicou no link foi o usuário).
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(pid);
    }
    if s.write_all(format!("{code}\n").as_bytes()).is_err() {
        return false;
    }
    // Espera o "ok": se a porta era de outro programa, este app abre normalmente.
    let mut buf = [0u8; 2];
    s.read_exact(&mut buf).is_ok() && &buf == b"ok"
}

/// Escuta códigos mandados por apps abertos depois (por links). Cada código vai para
/// `on_code` (numa thread própria).
pub fn listen(on_code: impl Fn(String) + Send + 'static) {
    if let Some(p) = instance_file() {
        listen_via(&p, on_code);
    }
}

fn listen_via(file: &std::path::Path, on_code: impl Fn(String) + Send + 'static) {
    let Ok(l) = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) else { return };
    let Ok(port) = l.local_addr().map(|a| a.port()) else { return };
    let _ = std::fs::write(file, format!("{port} {}", std::process::id()));
    std::thread::Builder::new()
        .name("aj-convite".into())
        .spawn(move || {
            for mut s in l.incoming().flatten() {
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = Vec::new();
                let mut b = [0u8; 1];
                while buf.len() <= MAX_CODE && s.read(&mut b).is_ok_and(|n| n == 1) && b[0] != b'\n' {
                    buf.push(b[0]);
                }
                let code = String::from_utf8_lossy(&buf).into_owned();
                if code_from_link(&format!("{SCHEME}:{code}")).as_deref() == Some(code.as_str()) {
                    let _ = s.write_all(b"ok");
                    on_code(code);
                }
            }
        })
        .ok();
}

/// Registra o `antijanja://` no Windows para este executável (só na versão instalada, e
/// só se ainda não aponta para ele).
pub fn register() {
    if cfg!(debug_assertions) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let command = format!("\"{}\" \"%1\"", exe.display());
    if let Err(e) = write_registry(&command) {
        crate::connlog::log(format!("convite: não registrou o link antijanja:// ({e})"));
    }
}

fn write_registry(command: &str) -> windows::core::Result<()> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::System::Registry::*;
    let set = |path: &str, name: Option<&str>, value: &str| -> windows::core::Result<()> {
        let mut key = HKEY::default();
        unsafe {
            RegCreateKeyExW(HKEY_CURRENT_USER, &HSTRING::from(path), None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut key, None).ok()?;
            let data: Vec<u8> = value.encode_utf16().chain(std::iter::once(0)).flat_map(|c| c.to_le_bytes()).collect();
            let name = name.map(HSTRING::from);
            let r = RegSetValueExW(key, name.as_ref().map(|n| PCWSTR(n.as_ptr())).unwrap_or(PCWSTR::null()), None, REG_SZ, Some(&data)).ok();
            let _ = RegCloseKey(key);
            r
        }
    };
    let base = format!("Software\\Classes\\{SCHEME}");
    set(&base, None, "URL:AntiJanja")?;
    set(&base, Some("URL Protocol"), "")?;
    set(&format!("{base}\\shell\\open\\command"), None, command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_codes_from_links() {
        let code = "AJ3.abcDEF123-_xyz";
        assert_eq!(code_from_link(&link_for(code)).as_deref(), Some(code));
        assert_eq!(code_from_link(&format!("antijanja://{code}/")).as_deref(), Some(code));
        assert_eq!(code_from_link(&format!("\"ANTIJANJA:{code}\"")).as_deref(), Some(code));
        assert_eq!(code_from_link("antijanja://entrar/AJ3%2Eabc").as_deref(), Some("AJ3.abc"));
        assert_eq!(code_from_link("https://exemplo.com/AJ3.abc"), None);
        assert_eq!(code_from_link("antijanja://entrar/"), None);
        assert_eq!(code_from_link("antijanja://entrar/AJ3.a<script>"), None);
        assert_eq!(code_from_link(&format!("antijanja://entrar/{}", "a".repeat(200))), None);
        assert_eq!(code_from_link("C:\\Program Files\\AntiJanja\\antijanja.exe"), None);
    }

    /// Um segundo app passa o código para o primeiro e recebe o "ok".
    #[test]
    fn second_instance_hands_the_code_over() {
        let file = std::env::temp_dir().join(format!("aj-instancia-teste-{}.txt", std::process::id()));
        assert!(!forward_via(&file, "AJ3.abc"), "sem app aberto");
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        listen_via(&file, move |c| {
            let _ = tx.lock().unwrap().send(c);
        });
        assert!(forward_via(&file, "AJ3.abc"));
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "AJ3.abc");
        assert!(!forward_via(&file, "AJ3.a b<"), "código inválido não passa");
        let _ = std::fs::remove_file(file);
    }
}
