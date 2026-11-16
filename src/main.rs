//! AntiJanja — compartilhamento de tela P2P, sem servidor central.
//!
//! Pipeline 100% nativo:
//! - captura: Windows Graphics Capture (`capture`)
//! - conversão + codificação H.264 por hardware: Media Foundation (`media`, `video`)
//! - transporte: WebRTC nativo, webrtc-rs (`transport`), bitrate adaptativo (`congestion`)
//! - encontro dos peers: DHT do BitTorrent, criptografado (`rendezvous`)
//! - áudio: WASAPI com controle por aplicativo + Opus (`audio`)
//! - exibição: wgpu numa janela-filha nativa (`player`), geração de quadros com a
//!   NVIDIA Optical Flow (`nvof`, `framegen`)
//! - interface: Slint (`ui`, arquivos em `ui/`), sem navegador embutido
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agenda;
mod app;
mod audio;
mod capture;
mod framegen;
mod congestion;
mod gpu;
mod media;
mod mesh;
mod nvof;
mod pieces;
mod player;
mod playout;
mod rendezvous;
mod session;
mod settings;
mod transport;
mod ui;
mod updater;
mod video;

fn main() {
    if let Err(e) = ui::run() {
        eprintln!("erro ao iniciar o AntiJanja: {e}");
    }
}
