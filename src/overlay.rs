//! Janela dos controles por cima do vídeo (`ui/overlay.slint`).
//!
//! O vídeo é uma janela-filha nativa (`player`), e a janela principal não consegue
//! desenhar nada por cima dela. Os controles ficam numa segunda janela Slint,
//! transparente, que tem a principal como dona: fica sempre na frente dela, minimiza junto,
//! não aparece na barra de tarefas e nunca é ativada (o teclado continua na principal).
//! Ela cobre a área interna da principal e acompanha quando a principal mexe. A região da
//! janela fica só nas faixas de controles visíveis e no menu: fora delas a janela não
//! existe para o mouse, que vai direto para o vídeo.

use slint::winit_030::winit;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CombineRgn, CreateRectRgn, CreateRoundRectRgn, DeleteObject, GetMonitorInfoW, MonitorFromWindow, SetWindowRgn, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, RGN_OR,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Retângulo da região, em pixels físicos relativos à janela: x, y, largura, altura e
/// raio dos cantos.
pub type Rect = (i32, i32, i32, i32, i32);

thread_local! {
    /// Dona da próxima janela criada (ver `prepare`).
    static PENDING_OWNER: Cell<Option<isize>> = const { Cell::new(None) };
    /// Última região aplicada (para não refazer à toa a cada atualização da tela).
    static LAST_REGION: RefCell<Option<Vec<Rect>>> = const { RefCell::new(None) };
}
static OVERLAY: AtomicIsize = AtomicIsize::new(0);

/// Gancho dos atributos das janelas do Slint: a primeira janela criada depois de
/// `prepare` vira a dos controles.
pub fn attributes_hook(attrs: winit::window::WindowAttributes) -> winit::window::WindowAttributes {
    use winit::platform::windows::WindowAttributesExtWindows;
    match PENDING_OWNER.with(|p| p.take()) {
        Some(owner) => attrs.with_owner_window(owner).with_skip_taskbar(true).with_decorations(false).with_active(false),
        None => attrs,
    }
}

/// Chamar logo antes de criar a janela dos controles (o Slint cria a janela nativa ao
/// criar o componente).
pub fn prepare(owner: isize) {
    PENDING_OWNER.with(|p| p.set(Some(owner)));
}

/// Depois de criar a janela (deu certo ou não): as próximas janelas são normais.
pub fn prepare_done() {
    PENDING_OWNER.with(|p| p.set(None));
}

/// Liga a janela dos controles à principal, depois de as duas existirem.
pub fn attach(main: isize, overlay: isize) {
    OVERLAY.store(overlay, Ordering::Relaxed);
    unsafe {
        let h = HWND(overlay as *mut _);
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        SetWindowLongPtrW(h, GWL_EXSTYLE, ex | (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as isize);
        let _ = SetWindowSubclass(HWND(main as *mut _), Some(follow_main), 1, 0);
    }
    set_region(&[]);
    follow(main);
}

unsafe extern "system" fn follow_main(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    let r = DefSubclassProc(hwnd, msg, wp, lp);
    if msg == WM_WINDOWPOSCHANGED || msg == WM_SIZE {
        follow(hwnd.0 as isize);
    }
    r
}

/// Põe a janela dos controles exatamente sobre a área interna da principal.
fn follow(main: isize) {
    let overlay = OVERLAY.load(Ordering::Relaxed);
    if overlay == 0 {
        return;
    }
    unsafe {
        let main = HWND(main as *mut _);
        let mut r = RECT::default();
        let _ = GetClientRect(main, &mut r);
        let mut p = POINT::default();
        let _ = ClientToScreen(main, &mut p);
        let (w, mut h) = (r.right.max(1), r.bottom.max(1));
        // Em tela cheia a janela cobriria o monitor inteiro, e o Windows passa a tratar uma
        // janela OpenGL sem bordas que cobre o monitor como "tela cheia": deixa de compô-la
        // com transparência e o que não tem controle vira preto por cima do vídeo. Um pixel
        // a menos de altura evita isso (a última linha não tem controle nenhum).
        let mon = MonitorFromWindow(main, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            let m = mi.rcMonitor;
            if p.x <= m.left && p.y <= m.top && p.x + w >= m.right && p.y + h >= m.bottom {
                h = (m.bottom - p.y - 1).max(1);
            }
        }
        let _ = SetWindowPos(
            HWND(overlay as *mut _),
            None,
            p.x,
            p.y,
            w,
            h,
            SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER,
        );
    }
}

/// Recorta a janela dos controles nos retângulos dados (vazio = janela invisível e o
/// mouse passa direto).
pub fn set_region(rects: &[Rect]) {
    let overlay = OVERLAY.load(Ordering::Relaxed);
    if overlay == 0 {
        return;
    }
    let same = LAST_REGION.with(|l| l.borrow().as_deref() == Some(rects));
    if same {
        return;
    }
    LAST_REGION.with(|l| *l.borrow_mut() = Some(rects.to_vec()));
    unsafe {
        let region = CreateRectRgn(0, 0, 0, 0);
        for &(x, y, w, h, radius) in rects {
            let part = if radius > 0 {
                CreateRoundRectRgn(x, y, x + w + 1, y + h + 1, radius * 2, radius * 2)
            } else {
                CreateRectRgn(x, y, x + w, y + h)
            };
            CombineRgn(Some(region), Some(region), Some(part), RGN_OR);
            let _ = DeleteObject(part.into());
        }
        // A janela passa a ser dona da região (não apagar `region`).
        SetWindowRgn(HWND(overlay as *mut _), Some(region), true);
    }
}
