//! Ponte entre o motor e a interface (substitui o `AppHandle` do Tauri).
//!
//! O motor emite eventos com nome + dados em JSON (como antes); a interface os recebe
//! na thread dela. Também guarda o estado da DHT e a janela principal (para o player).

use crate::rendezvous::RdvState;
use serde::Serialize;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;

type EventFn = dyn Fn(&str, serde_json::Value) + Send + Sync;

#[derive(Clone)]
pub struct AppHandle(Arc<Inner>);

struct Inner {
    on_event: Box<EventFn>,
    rdv: RdvState,
    hwnd: AtomicIsize,
}

impl AppHandle {
    pub fn new(on_event: impl Fn(&str, serde_json::Value) + Send + Sync + 'static) -> Self {
        AppHandle(Arc::new(Inner { on_event: Box::new(on_event), rdv: RdvState::default(), hwnd: AtomicIsize::new(0) }))
    }

    /// Manda um evento para a interface (chega na thread da interface).
    pub fn emit<S: Serialize>(&self, event: &str, payload: S) -> Result<(), String> {
        let value = serde_json::to_value(payload).map_err(|e| e.to_string())?;
        (self.0.on_event)(event, value);
        Ok(())
    }

    pub fn rdv(&self) -> &RdvState {
        &self.0.rdv
    }

    /// HWND da janela principal (pai dos players nativos).
    pub fn hwnd(&self) -> isize {
        self.0.hwnd.load(Ordering::SeqCst)
    }

    pub fn set_hwnd(&self, hwnd: isize) {
        self.0.hwnd.store(hwnd, Ordering::SeqCst);
    }

    /// Roda `f` na thread da interface (onde as janelas nativas precisam ser criadas).
    pub fn run_on_main_thread(&self, f: impl FnOnce() + Send + 'static) -> Result<(), String> {
        slint::invoke_from_event_loop(f).map_err(|e| e.to_string())
    }
}
