//! Preferências salvas em `%APPDATA%\AntiJanja\settings.json`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub name: String,
    /// Não usado: o app sempre abre no tema do Windows (mantido por compatibilidade).
    pub theme: String,
    pub res: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub cursor: bool,
    /// "" = ainda não escolhido (na primeira vez decidimos pelo Discord)
    pub audio_mode: String,
    pub audio_app: String,
    pub abitrate: u32,
    pub volume: u32,
    pub public: bool,
    pub view_volume: u32,
    /// fps para assistir (0 = o da stream). Acima do da stream, gera quadros
    /// intermediários (placa NVIDIA); abaixo, descarta.
    pub view_fps: u32,
    /// Senha da transmissão (vazia = sem senha). Quem assiste precisa do código e dela.
    pub stream_password: String,
    /// Malha P2P (experimental): quem assiste repassa partes do vídeo para os outros.
    pub mesh: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            name: String::new(),
            theme: String::new(),
            res: 1080,
            fps: 60,
            bitrate: 0,
            cursor: true,
            audio_mode: String::new(),
            audio_app: String::new(),
            abitrate: 192,
            volume: 100,
            // Primeira vez: "Só com código" (depois vale a última escolha).
            public: false,
            view_volume: 100,
            view_fps: 0,
            stream_password: String::new(),
            mesh: false,
        }
    }
}

fn path() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("APPDATA")?).join("AntiJanja");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("settings.json"))
}

pub fn load() -> Settings {
    path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(s: &Settings) {
    if let (Some(p), Ok(json)) = (path(), serde_json::to_vec_pretty(s)) {
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(tmp, p);
        }
    }
}

/// O Windows está no tema escuro (para apps)?
pub fn windows_prefers_dark() -> bool {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    ok.is_ok() && value == 0
}
