//! AntiJanja — compartilhamento de tela P2P, sem servidor central.
//!
//! Pipeline 100% nativo:
//! - captura: Windows Graphics Capture (`capture`)
//! - conversão + codificação H.264 por hardware: Media Foundation (`media`, `video`)
//! - transporte: WebRTC nativo, webrtc-rs (`transport`)
//! - encontro dos peers: DHT do BitTorrent, criptografado (`rendezvous`)
//! - áudio: WASAPI com controle por aplicativo + Opus (`audio`)
//! - exibição: wgpu numa janela-filha nativa (`player`)
//! A interface (React) só controla; o vídeo não passa pelo WebView.

mod audio;
mod capture;
mod gpu;
mod media;
mod player;
mod playout;
mod rendezvous;
mod session;
mod transport;
mod video;

#[tauri::command]
fn device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Meu PC".into())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(rendezvous::RdvState::default())
        .setup(|app| {
            rendezvous::init(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            device_name,
            rendezvous::rdv_ready,
            rendezvous::public_list,
            audio::audio_apps,
            session::sources_list,
            session::host_start,
            session::host_update,
            session::host_new_code,
            session::host_kick,
            session::host_stop,
            session::viewer_join,
            session::viewer_leave,
            session::player_rect,
            session::viewer_volume,
        ])
        .run(tauri::generate_context!())
        .expect("erro ao iniciar o AntiJanja");
}
