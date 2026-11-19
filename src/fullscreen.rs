//! Tela cheia sem bordas feita direto no Windows.
//!
//! A tela cheia do winit troca o estilo da janela (tira a barra de título e a borda), e na
//! troca o Windows desenhava por um instante a borda clássica da janela (cinza-claro e
//! azul, com os botões no estilo antigo): um clarão em volta da janela, sobretudo nos
//! cantos, ao entrar e ao sair. Trocar o estilo e o tamanho no mesmo `SetWindowPos` não
//! resolve: o clarão vem da troca de estilo.
//!
//! Aqui o estilo nunca muda. Em tela cheia, a janela ocupa o monitor e responde ao Windows
//! que a borda e a barra de título têm tamanho zero (`WM_NCCALCSIZE`): a área interna é a
//! janela inteira. Os cantos arredondados e a linha de borda do Windows 11 ficam
//! desligados enquanto isso.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DEFAULT, DWMWCP_DONOTROUND};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Em tela cheia agora?
static ON: AtomicBool = AtomicBool::new(false);
/// Posição de antes da tela cheia, para voltar.
static SAVED: Mutex<Option<WINDOWPLACEMENT>> = Mutex::new(None);

/// "Sem cor" para a linha de borda do Windows 11.
const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;

unsafe extern "system" fn no_frame(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    // Área interna = a janela inteira (sem barra de título nem borda).
    if msg == WM_NCCALCSIZE && wp.0 != 0 && ON.load(Ordering::SeqCst) {
        return LRESULT(0);
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}

/// Liga ou desliga a tela cheia da janela `hwnd` (thread principal). Ao desligar, quem
/// chama repinta a borda com as cores do tema (a linha de borda fica sem cor em tela cheia).
pub fn set(hwnd: isize, on: bool) {
    let h = HWND(hwnd as *mut _);
    let mut saved = SAVED.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        static HOOKED: AtomicBool = AtomicBool::new(false);
        if !HOOKED.swap(true, Ordering::SeqCst) {
            let _ = SetWindowSubclass(h, Some(no_frame), 0x4653, 0);
        }
        if on {
            if saved.is_some() {
                return;
            }
            let mut place = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            if GetWindowPlacement(h, &mut place).is_err() || !GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
                return;
            }
            *saved = Some(place);
            ON.store(true, Ordering::SeqCst);
            let corner = DWMWCP_DONOTROUND;
            let _ = DwmSetWindowAttribute(h, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as *const _, 4);
            let none = DWMWA_COLOR_NONE;
            let _ = DwmSetWindowAttribute(h, DWMWA_BORDER_COLOR, &none as *const _ as *const _, 4);
            let m = mi.rcMonitor;
            if place.showCmd == SW_SHOWMAXIMIZED.0 as u32 {
                // Maximizada, o Windows prende a janela no retângulo de maximizada e ignora o
                // tamanho novo. Passa para "normal" já do tamanho do monitor, num passo só (a
                // posição "normal" é em coordenadas da área de trabalho, sem a barra de tarefas).
                let w = mi.rcWork;
                let (dx, dy) = (w.left - m.left, w.top - m.top);
                let mut full = place;
                full.showCmd = SW_SHOWNORMAL.0 as u32;
                full.flags = WINDOWPLACEMENT_FLAGS(0);
                full.rcNormalPosition = windows::Win32::Foundation::RECT { left: m.left - dx, top: m.top - dy, right: m.right - dx, bottom: m.bottom - dy };
                let _ = SetWindowPlacement(h, &full);
            }
            // Sem SWP_NOOWNERZORDER: a janela dos controles (dona = esta) precisa subir junto,
            // senão fica atrás do vídeo durante toda a tela cheia.
            let _ = SetWindowPos(h, Some(HWND_TOP), m.left, m.top, m.right - m.left, m.bottom - m.top, SWP_FRAMECHANGED);
        } else {
            let Some(place) = saved.take() else { return };
            ON.store(false, Ordering::SeqCst);
            let corner = DWMWCP_DEFAULT;
            let _ = DwmSetWindowAttribute(h, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as *const _, 4);
            let _ = SetWindowPlacement(h, &place);
            let _ = SetWindowPos(h, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
        }
    }
}
