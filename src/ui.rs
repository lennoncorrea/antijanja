//! Controlador da interface: liga a janela Slint ao motor.
//!
//! Faz o papel que antes era do `engine.ts` + `store.ts`: guarda o estado da interface,
//! recebe os eventos do motor (sempre na thread da interface), chama o motor em threads
//! próprias e posiciona os players nativos sobre os tiles da tela "Assistindo".

use crate::app::AppHandle;
use crate::{audio, capture, overlay, rendezvous, session, settings, transport, updater};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

slint::include_modules!();

// Medidas fixas da tela "Assistindo" (iguais às do watch.slint).
const TOPBAR: f32 = 44.0;
const ADDBAR: f32 = 60.0;
const PAD: f32 = 12.0;
const GAP: f32 = 12.0;
/// Cabeçalho do tile enquanto conecta.
const HEADER: f32 = 44.0;
/// Faixas dos controles por cima do vídeo (iguais às do overlay.slint).
const TOP_BAND: f32 = 76.0;
const BOTTOM_BAND: f32 = 84.0;
/// Raio dos cantos do vídeo dentro do tile (o tile tem 12 e borda de 1).
const VIDEO_RADIUS: f32 = 11.0;
/// Largura do menu de engrenagem (igual ao `GearMenu` do Slint).
const MENU_W: f32 = 300.0;
/// Raio dos cantos do menu de engrenagem (igual ao `border-radius` do GearMenu).
const MENU_RADIUS: f32 = 12.0;
/// Distância da base do menu até o fim do vídeo (o menu fica acima da linha de botões).
const MENU_GAP: f32 = 52.0;

const PUBLIC_REFRESH: Duration = Duration::from_secs(20);
/// Releitura do mural na DHT quando a fofoca está conectada (docs/gossip-ao-vivo.md).
const PUBLIC_REFRESH_GOSSIP: Duration = Duration::from_secs(120);
const TOAST_TIME: Duration = Duration::from_secs(4);
/// Controles por cima do vídeo: somem depois desse tempo com o mouse parado, ou deste
/// outro depois de o mouse sair do vídeo.
const CONTROLS_TIME: Duration = Duration::from_millis(2500);
const CONTROLS_LEAVE: Duration = Duration::from_millis(700);
/// Tempo do esmaecer dos controles: a janela deles só perde a região depois disso.
const CONTROLS_FADE: Duration = Duration::from_millis(250);

struct Viewer {
    id: String,
    code: String,
    host_name: String,
    step: i32,
    /// O host respondeu que está ocupado com outras entradas (o pedido é repetido sozinho).
    busy: bool,
    watching: bool,
    stats: Option<Value>,
    ping: Option<u32>,
    volume: u32,
    muted: bool,
    /// Qualidade pedida ao host: "auto" ou a altura ("720").
    quality: String,
    /// Alturas que o host oferece além da principal (vazio = só uma).
    renditions: Vec<u32>,
    /// fps da stream para o menu: o medido oscila (57, 60, 61...) e mudava os textos do
    /// menu a cada segundo. Arredondado para uma taxa comum e só troca depois de 2
    /// medições seguidas concordarem.
    nominal_fps: u32,
    fps_candidate: (u32, u8),
}

/// Taxa comum mais perto do fps medido (dentro de 10%), senão o múltiplo de 5 mais perto.
fn snap_fps(measured: f64) -> u32 {
    const COMMON: [u32; 13] = [24, 25, 30, 48, 50, 60, 75, 90, 100, 120, 144, 165, 240];
    if measured < 1.0 {
        return 0;
    }
    COMMON
        .iter()
        .copied()
        .min_by(|a, b| (*a as f64 - measured).abs().total_cmp(&(*b as f64 - measured).abs()))
        .filter(|c| (*c as f64 - measured).abs() <= *c as f64 * 0.1)
        .unwrap_or(((measured / 5.0).round() * 5.0).max(1.0) as u32)
}

impl Viewer {
    fn update_nominal_fps(&mut self, measured: f64) {
        let snapped = snap_fps(measured);
        if self.nominal_fps == 0 {
            self.nominal_fps = snapped;
        }
        if snapped == self.nominal_fps {
            self.fps_candidate = (0, 0);
            return;
        }
        if self.fps_candidate.0 == snapped {
            self.fps_candidate.1 += 1;
        } else {
            self.fps_candidate = (snapped, 1);
        }
        if self.fps_candidate.1 >= 2 {
            self.nominal_fps = snapped;
            self.fps_candidate = (0, 0);
        }
    }
}

struct Ctl {
    ui: slint::Weak<AppWindow>,
    app: AppHandle,
    s: settings::Settings,
    /// Placa NVIDIA com Optical Flow (geração de quadros possível).
    framegen_available: bool,
    /// Stream com o menu de engrenagem aberto, e qual painel ("main", "fps", "quality").
    menu: Option<String>,
    menu_panel: String,
    /// Código esperando a senha (janela de senha aberta).
    password_for: Option<String>,
    view: String,
    net: String,
    live: bool,
    source: Option<capture::Source>,
    code: String,
    host_viewers: u32,
    apps: Vec<audio::AudioApp>,
    public: Option<Vec<rendezvous::PublicStream>>,
    public_busy: bool,
    public_updated: Option<Instant>,
    /// Anúncio mais novo de cada transmissão que chegou pela fofoca (docs/gossip-ao-vivo.md),
    /// para continuar valendo quando a lista é relida pela DHT (que pode estar atrasada).
    gossip: HashMap<String, rendezvous::PublicEntry>,
    sources: Option<Vec<capture::Source>>,
    thumbs: HashMap<String, Image>,
    picker: String,
    picker_tab: String,
    picker_selected: String,
    viewers: Vec<Viewer>,
    focus: Option<String>,
    fullscreen: bool,
    /// Stream com os controles à mostra, e até quando (o mouse mexeu sobre ela).
    hover: Option<(String, Instant)>,
    /// Último estado dos controles desenhado (ver `controls_state`).
    last_controls: (Option<String>, Option<String>),
    /// Janela dos controles por cima do vídeo (criada quando a principal já existe).
    overlay: Option<VideoOverlay>,
    overlay_attached: bool,
    toasts: Vec<(i32, String, String, Instant)>,
    toast_seq: i32,
    updating: bool,
    /// Gravar as configurações / mandar a configuração ao host já está marcado.
    save_pending: bool,
    push_pending: bool,
    /// Altura de cada painel do menu de engrenagem, medida pelo Slint.
    menu_heights: HashMap<String, f32>,
}

thread_local! {
    static CTL: RefCell<Option<Ctl>> = const { RefCell::new(None) };
}

/// Acesso ao estado e à janela (só na thread da interface).
fn with<R>(f: impl FnOnce(&mut Ctl, &AppWindow) -> R) -> Option<R> {
    CTL.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        let ctl = guard.as_mut()?;
        let ui = ctl.ui.upgrade()?;
        Some(f(ctl, &ui))
    })
}

/// Roda `f` na próxima volta do loop da interface (evita reentrância).
fn later(f: impl FnOnce(&mut Ctl, &AppWindow) + 'static) {
    slint::Timer::single_shot(Duration::ZERO, move || {
        with(f);
    });
}

/// Trabalho pesado numa thread; o resultado volta para a thread da interface.
fn bg<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(&mut Ctl, &AppWindow, T) + Send + 'static) {
    std::thread::spawn(move || {
        let r = work();
        let _ = slint::invoke_from_event_loop(move || {
            with(|c, ui| done(c, ui, r));
        });
    });
}

/* ================================================================ utilidades */

fn s(text: impl AsRef<str>) -> SharedString {
    SharedString::from(text.as_ref())
}

/// Grava a propriedade só se o valor mudou. Gravar o mesmo valor também avisa o Slint,
/// que reavalia o que depende dela: com `if` (como os painéis do menu de engrenagem), os
/// itens eram recriados a cada atualização, e no meio disso o menu chegou a medir 12 px
/// (o recorte no vídeo ficava desse tamanho e o vídeo cobria o menu).
macro_rules! set_if_changed {
    ($a:expr, $get:ident, $set:ident, $v:expr) => {{
        let v = $v;
        if $a.$get() != v {
            $a.$set(v);
        }
    }};
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

/// Atualiza a lista mostrada no lugar, mexendo só nas linhas que mudaram. Trocar o
/// modelo inteiro (como era feito, a cada segundo nas streams) faz o Slint recriar todos
/// os itens: um clique no meio, o volume sendo arrastado e o hover se perdiam.
fn sync<T: Clone + PartialEq + 'static>(current: ModelRc<T>, new: Vec<T>, set: impl FnOnce(ModelRc<T>)) {
    let Some(vm) = current.as_any().downcast_ref::<VecModel<T>>() else { return set(model(new)) };
    let n = new.len();
    for (i, item) in new.into_iter().enumerate() {
        if i >= vm.row_count() {
            vm.push(item);
        } else if vm.row_data(i).as_ref() != Some(&item) {
            vm.set_row_data(i, item);
        }
    }
    while vm.row_count() > n {
        vm.remove(vm.row_count() - 1);
    }
}

/// `sync` para listas de linhas que têm a própria lista de itens dentro (cada linha
/// mantém o modelo dela; só os itens que mudaram são trocados).
fn sync_rows<R: Clone + PartialEq + 'static, T: Clone + PartialEq + 'static>(
    current: ModelRc<R>,
    new: Vec<Vec<T>>,
    items: impl Fn(&R) -> ModelRc<T>,
    make: impl Fn(ModelRc<T>) -> R,
    set: impl FnOnce(ModelRc<R>),
) {
    let Some(vm) = current.as_any().downcast_ref::<VecModel<R>>() else {
        return set(model(new.into_iter().map(|v| make(model(v))).collect()));
    };
    let n = new.len();
    for (i, row) in new.into_iter().enumerate() {
        match vm.row_data(i) {
            Some(old) => sync(items(&old), row, |m| vm.set_row_data(i, make(m))),
            None => vm.push(make(model(row))),
        }
    }
    while vm.row_count() > n {
        vm.remove(vm.row_count() - 1);
    }
}

fn initials(name: &str) -> String {
    let out: String = name.split_whitespace().take(2).filter_map(|w| w.chars().next()).collect::<String>().to_uppercase();
    if out.is_empty() { "?".into() } else { out }
}

fn cover_index(name: &str) -> i32 {
    let mut h: i32 = 0;
    for c in name.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(c as i32);
    }
    (h.unsigned_abs() % 3) as i32 + 1
}

fn since(ms: i64) -> String {
    let min = ((rendezvous::now_ms() - ms) / 60_000).max(0);
    match min {
        0 => "começou agora".into(),
        1..=59 => format!("há {min} min"),
        _ if min % 60 == 0 => format!("há {} h", min / 60),
        _ => format!("há {} h {} min", min / 60, min % 60),
    }
}

fn format_rate(kbps: f64) -> String {
    if kbps >= 1000.0 { format!("{:.1} Mb/s", kbps / 1000.0).replace('.', ",") } else { format!("{} kb/s", kbps.round() as i64) }
}

fn decode_data_url(url: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let b64 = url.split_once(',')?.1;
    let bytes = STANDARD.decode(b64).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(img.as_raw(), img.width(), img.height()))
}

fn random_id() -> String {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn clipboard_get() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok().map(|t| t.trim().to_string())
}

fn clipboard_set(text: &str) -> bool {
    arboard::Clipboard::new().and_then(|mut c| c.set_text(text.to_string())).is_ok()
}

fn app_name(c: &Ctl, exe: &str) -> String {
    c.apps.iter().find(|a| a.exe == exe).map(|a| a.name.clone()).unwrap_or_else(|| exe.trim_end_matches(".exe").to_string())
}

/* ============================================================ janela nativa */

fn hwnd_of(ui: &AppWindow) -> Option<isize> {
    window_hwnd(ui.window())
}

/// Tamanho da área interna da janela, em pixels físicos.
fn client_size(hwnd: isize) -> Option<(u32, u32)> {
    let mut r = windows::Win32::Foundation::RECT::default();
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetClientRect(windows::Win32::Foundation::HWND(hwnd as *mut _), &mut r).ok()? };
    (r.right > 0 && r.bottom > 0).then_some((r.right as u32, r.bottom as u32))
}

fn window_hwnd(window: &slint::Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window_handle();
    let wh = handle.window_handle().ok()?;
    match wh.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// Pinta a barra de título do Windows com as cores do tema (Windows 11).
fn paint_titlebar(hwnd: isize, dark: bool) {
    use windows::core::BOOL;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE};
    let h = HWND(hwnd as *mut _);
    let rgb = |r: u32, g: u32, b: u32| r | (g << 8) | (b << 16);
    let (caption, text, border) = if dark { (rgb(0x04, 0x0a, 0x0b), rgb(0xe8, 0xf1, 0xf3), rgb(0x17, 0x29, 0x2d)) } else { (rgb(0xf4, 0xfa, 0xfb), rgb(0x0c, 0x15, 0x17), rgb(0xd5, 0xe5, 0xe8)) };
    unsafe {
        let d = BOOL::from(dark);
        let _ = DwmSetWindowAttribute(h, DWMWA_USE_IMMERSIVE_DARK_MODE, &d as *const _ as *const _, 4);
        let _ = DwmSetWindowAttribute(h, DWMWA_CAPTION_COLOR, &caption as *const _ as *const _, 4);
        let _ = DwmSetWindowAttribute(h, DWMWA_TEXT_COLOR, &text as *const _ as *const _, 4);
        let _ = DwmSetWindowAttribute(h, DWMWA_BORDER_COLOR, &border as *const _ as *const _, 4);
    }
}

/* ================================================================ render */

fn render_general(c: &Ctl, ui: &AppWindow) {
    let a = ui.global::<App>();
    set_if_changed!(a, get_view, set_view, s(&c.view));
    set_if_changed!(a, get_net, set_net, s(&c.net));
    set_if_changed!(a, get_live, set_live, c.live);
    set_if_changed!(a, get_host_viewers, set_host_viewers, c.host_viewers as i32);
    set_if_changed!(a, get_watch_count, set_watch_count, c.viewers.len() as i32);
    set_if_changed!(a, get_fullscreen, set_fullscreen, c.fullscreen);
    set_if_changed!(a, get_title, set_title, s(if c.host_viewers > 0 { format!("AntiJanja ({} assistindo)", c.host_viewers) } else { "AntiJanja".into() }));
}

fn render_settings(c: &Ctl, ui: &AppWindow) {
    let a = ui.global::<App>();
    let st = &c.s;
    set_if_changed!(a, get_s_password_set, set_s_password_set, !st.stream_password.is_empty());
    set_if_changed!(a, get_s_res, set_s_res, s(st.res.to_string()));
    set_if_changed!(a, get_s_fps, set_s_fps, s(st.fps.to_string()));
    set_if_changed!(a, get_s_bitrate, set_s_bitrate, s(st.bitrate.to_string()));
    set_if_changed!(a, get_s_cursor, set_s_cursor, st.cursor);
    set_if_changed!(a, get_s_mesh, set_s_mesh, st.mesh);
    set_if_changed!(a, get_s_audio_mode, set_s_audio_mode, s(if st.audio_mode.is_empty() { "system" } else { &st.audio_mode }));
    set_if_changed!(a, get_s_audio_app, set_s_audio_app, s(&st.audio_app));
    set_if_changed!(a, get_s_abitrate, set_s_abitrate, s(st.abitrate.to_string()));
    set_if_changed!(a, get_s_volume, set_s_volume, st.volume as i32);
    set_if_changed!(a, get_s_public, set_s_public, st.public);

    let mut options: Vec<SelectOption> = c
        .apps
        .iter()
        .map(|app| SelectOption { value: s(&app.exe), label: s(if app.active { format!("{} · tocando agora", app.name) } else { app.name.clone() }) })
        .collect();
    if !st.audio_app.is_empty() && !c.apps.iter().any(|app| app.exe == st.audio_app) {
        options.push(SelectOption { value: s(&st.audio_app), label: s(format!("{} (fechado)", app_name(c, &st.audio_app))) });
    }
    sync(a.get_app_options(), options, |m| a.set_app_options(m));
    sync(a.get_apps(), c.apps.iter().map(|x| AudioApp { exe: s(&x.exe), name: s(&x.name), active: x.active }).collect(), |m| a.set_apps(m));
}

fn render_public(c: &Ctl, ui: &AppWindow) {
    let a = ui.global::<App>();
    set_if_changed!(a, get_public_busy, set_public_busy, c.public_busy);
    set_if_changed!(a, get_public_loading, set_public_loading, c.public.is_none());
    a.set_public_updated(s(if c.public_busy {
        "Atualizando".to_string()
    } else if crate::gossip::peer_count() > 0 {
        // Conectado à fofoca: a lista atualiza sozinha, na hora.
        "ao vivo".into()
    } else {
        match c.public_updated.map(|t| t.elapsed().as_secs()) {
            None => "Atualizar".into(),
            Some(n) if n < 5 => "agora".into(),
            Some(n) => format!("há {n} s"),
        }
    }));
    let list = c.public.as_deref().unwrap_or(&[]);
    let others: Vec<&rendezvous::PublicStream> = list.iter().filter(|p| !p.own).collect();
    set_if_changed!(a, get_public_mine, set_public_mine, list.iter().any(|p| p.own));
    set_if_changed!(a, get_public_count, set_public_count, others.len() as i32);
    set_if_changed!(a, get_public_watching, set_public_watching, others.iter().map(|p| p.entry.viewers as i32).sum());
    let to_stream = |p: &rendezvous::PublicStream| {
        let e = &p.entry;
        Stream {
            name: s(&e.name),
            title: s(if e.title.is_empty() { "Tela" } else { &e.title }),
            code: s(&e.code),
            viewers: e.viewers as i32,
            quality: s(&e.quality),
            since: s(since(e.since)),
            initials: s(initials(&e.name)),
            cover: cover_index(&e.name),
            watching: c.viewers.iter().any(|v| v.code == e.code),
            blank: false,
            locked: e.locked,
        }
    };
    set_if_changed!(a, get_has_featured, set_has_featured, !others.is_empty());
    if let Some(first) = others.first() {
        set_if_changed!(a, get_featured, set_featured, to_stream(first));
    }
    let rows: Vec<Vec<Stream>> = others
        .iter()
        .skip(1)
        .copied()
        .collect::<Vec<_>>()
        .chunks(3)
        .map(|chunk| {
            let mut items: Vec<Stream> = chunk.iter().map(|p| to_stream(p)).collect();
            items.resize_with(3, || Stream { blank: true, ..Default::default() });
            items
        })
        .collect();
    sync_rows(a.get_rows(), rows, |r| r.items.clone(), |items| StreamRow { items }, |m| a.set_rows(m));
}

fn render_watching_list(c: &Ctl, ui: &AppWindow) {
    const STEP_TEXT: [&str; 4] = ["Preparando", "Procurando o host", "Esperando o host", "Conectando"];
    let items: Vec<WatchingItem> = c
        .viewers
        .iter()
        .map(|v| {
            let detail = if v.watching {
                v.ping.map(|p| format!("ao vivo · {p} ms")).unwrap_or_else(|| "ao vivo".into())
            } else {
                if v.busy { "Host ocupado, tentando de novo".to_string() } else { STEP_TEXT[v.step.clamp(0, 3) as usize].to_string() }
            };
            let who = if v.host_name.is_empty() { &v.code } else { &v.host_name };
            WatchingItem {
                id: s(&v.id),
                name: s(if v.host_name.is_empty() { "Conectando…" } else { &v.host_name }),
                initials: s(initials(if v.host_name.is_empty() { "?" } else { &v.host_name })),
                cover: cover_index(who),
                detail: s(detail),
            }
        })
        .collect();
    let a = ui.global::<App>();
    sync(a.get_watching_list(), items, |m| a.set_watching_list(m));
}

fn render_picker(c: &mut Ctl, ui: &AppWindow) {
    let a = ui.global::<App>();
    set_if_changed!(a, get_picker, set_picker, s(&c.picker));
    set_if_changed!(a, get_picker_tab, set_picker_tab, s(&c.picker_tab));
    set_if_changed!(a, get_picker_selected, set_picker_selected, s(&c.picker_selected));
    set_if_changed!(a, get_sources_loading, set_sources_loading, c.sources.is_none());
    let list = c.sources.as_deref().unwrap_or(&[]);
    set_if_changed!(a, get_count_monitors, set_count_monitors, list.iter().filter(|x| x.kind == "monitor").count() as i32);
    set_if_changed!(a, get_count_windows, set_count_windows, list.iter().filter(|x| x.kind == "window").count() as i32);
    let shown: Vec<&capture::Source> = list.iter().filter(|x| x.kind == c.picker_tab).collect();
    let cols = if c.picker_tab == "monitor" && shown.len() <= 2 { 2 } else { 3 };
    let current = if c.picker == "switch" { c.source.as_ref().map(|x| x.id.clone()) } else { None };
    let rows: Vec<Vec<Source>> = shown
        .chunks(cols)
        .map(|chunk| {
            let mut items: Vec<Source> = chunk
                .iter()
                .map(|x| {
                    let thumb = c.thumbs.get(&x.id).cloned();
                    Source {
                        id: s(&x.id),
                        name: s(&x.name),
                        detail: s(if x.kind == "window" && !x.app.is_empty() { format!("{}, {}×{}", x.app, x.width, x.height) } else { format!("{}×{}", x.width, x.height) }),
                        has_thumb: thumb.is_some(),
                        thumb: thumb.unwrap_or_default(),
                        is_window: x.kind == "window",
                        primary: x.primary,
                        current: current.as_deref() == Some(x.id.as_str()),
                        blank: false,
                    }
                })
                .collect();
            items.resize_with(cols, || Source { blank: true, ..Default::default() });
            items
        })
        .collect();
    sync_rows(a.get_source_rows(), rows, |r| r.items.clone(), |items| SourceRow { items }, |m| a.set_source_rows(m));
}

fn render_host_code(c: &Ctl, ui: &AppWindow) {
    let a = ui.global::<App>();
    set_if_changed!(a, get_code, set_code, s(&c.code));
    let (prefix, body) = if c.code.len() > 4 { c.code.split_at(4) } else { (c.code.as_str(), "") };
    set_if_changed!(a, get_code_prefix, set_code_prefix, s(prefix));
    let groups: Vec<String> = body.chars().collect::<Vec<_>>().chunks(4).map(|g| g.iter().collect()).collect();
    set_if_changed!(a, get_code_body, set_code_body, s(groups.join(" ")));
    set_if_changed!(a, get_source_name, set_source_name, s(c.source.as_ref().map(|x| x.name.as_str()).unwrap_or("")));
}

fn render_toasts(c: &Ctl, ui: &AppWindow) {
    let items: Vec<Toast> = c.toasts.iter().map(|(id, text, kind, _)| Toast { id: *id, text: s(text), kind: s(kind) }).collect();
    let a = ui.global::<App>();
    sync(a.get_toasts(), items, |m| a.set_toasts(m));
}

fn toast(c: &mut Ctl, ui: &AppWindow, kind: &str, text: impl Into<String>) {
    c.toast_seq += 1;
    c.toasts.push((c.toast_seq, text.into(), kind.into(), Instant::now() + TOAST_TIME));
    if c.toasts.len() > 4 {
        c.toasts.remove(0);
    }
    render_toasts(c, ui);
}

fn grid_for(n: usize) -> (usize, usize) {
    match n {
        0 | 1 => (1, 1),
        2 => (2, 1),
        3 | 4 => (2, 2),
        5 | 6 => (3, 2),
        _ => (3, n.div_ceil(3)),
    }
}

/// Recalcula a grade da tela "Assistindo" e reposiciona os players nativos.
fn relayout(c: &Ctl, ui: &AppWindow) {
    let t0 = Instant::now();
    relayout_inner(c, ui);
    if trace() {
        eprintln!("[ui {:.0}] relayout {:.2} ms", tms(), t0.elapsed().as_secs_f64() * 1000.0);
    }
}

/// Milissegundos desde o início (para alinhar as linhas do diagnóstico).
fn tms() -> f64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}

/// Diagnóstico de tempos da interface (AJ_TRACE).
fn trace() -> bool {
    thread_local! { static ON: bool = std::env::var_os("AJ_TRACE").is_some(); }
    ON.with(|x| *x)
}

fn relayout_inner(c: &Ctl, ui: &AppWindow) {
    let a = ui.global::<App>();
    let scale = ui.window().scale_factor();
    // Tamanho da área interna direto do Windows: logo depois de a janela mudar (tela cheia,
    // por exemplo) ele já está certo, enquanto o do Slint só muda quando o aviso de tamanho
    // novo passa pelo laço de eventos. Com o do Slint, o primeiro quadro no tamanho novo
    // saía com os vídeos ainda no lugar velho.
    let size = hwnd_of(ui).and_then(client_size).unwrap_or_else(|| {
        let s = ui.window().size();
        (s.width, s.height)
    });
    let (w, h) = (size.0 as f32 / scale, size.1 as f32 / scale);
    let fs = c.fullscreen;
    let top = if fs { 0.0 } else { TOPBAR + ADDBAR };
    let pad = if fs { 0.0 } else { PAD };
    let gap = if fs { 0.0 } else { GAP };
    let area_w = (w - 2.0 * pad).max(1.0);
    let area_h = (h - top - pad).max(1.0);

    let shown: Vec<&Viewer> = match &c.focus {
        Some(f) => c.viewers.iter().filter(|v| &v.id == f).collect(),
        None => c.viewers.iter().collect(),
    };
    let (cols, rows) = grid_for(shown.len());
    let tile_w = (area_w - gap * (cols as f32 - 1.0)) / cols as f32;
    let tile_h = (area_h - gap * (rows as f32 - 1.0)) / rows as f32;
    let players_visible = c.view == "watch" && c.picker.is_empty() && !c.updating && c.password_for.is_none();

    // As janelas de vídeo mudam todas juntas no fim (ver `player::apply`).
    crate::player::begin_batch();
    // Streams escondidas (outra está ampliada): player fora da tela.
    for v in c.viewers.iter().filter(|v| !shown.iter().any(|x| x.id == v.id)) {
        session::player_rect(v.id.clone(), 0, 0, 1, 1, false, 0, false);
    }
    let (controls_on, controls_alive) = controls_state(c, Instant::now());
    let ov = c.overlay.as_ref().map(|o| o.global::<Ov>());
    let px = |v: f32| (v * scale).round() as i32;
    let mut region: Vec<overlay::Rect> = Vec::new();
    let mut tiles = Vec::new();
    let mut menu_tile = false;
    for (i, v) in shown.iter().enumerate() {
        let (col, row) = ((i % cols) as f32, (i / cols) as f32);
        let x = pad + col * (tile_w + gap);
        let y = row * (tile_h + gap);
        let border = if fs { 0.0 } else { 1.0 };
        // Assistindo, o vídeo ocupa o tile inteiro (os controles ficam por cima, em outra
        // janela). Conectando, há o cabeçalho.
        let head = if v.watching { border } else { HEADER };
        let (vx, vy) = (x + border, top + y + head);
        let (vw, vh) = (tile_w - 2.0 * border, (tile_h - head - border).max(1.0));
        let stats = v.stats.as_ref();
        let num = |k: &str| stats.and_then(|st| st[k].as_f64()).unwrap_or(0.0);
        let menu_here = c.menu.as_deref() == Some(v.id.as_str()) && v.watching;
        let chrome = v.watching && (menu_here || controls_on.as_deref() == Some(v.id.as_str()));

        // Faixas dos controles: a janela deles só existe ali (e continua um pouco depois de
        // sumirem, enquanto esmaecem).
        if v.watching && (menu_here || controls_alive.as_deref() == Some(v.id.as_str())) {
            let top_h = TOP_BAND.min(vh);
            let bottom_h = BOTTOM_BAND.min(vh);
            region.push((px(vx), px(vy), px(vw), px(top_h), 0));
            region.push((px(vx), px(vy + vh - bottom_h), px(vw), px(bottom_h), 0));
        }

        // Menu de engrenagem desta stream: acima da linha de botões, no canto direito.
        if menu_here {
            menu_tile = true;
            let m = gear_menu(c, ui, v);
            // Altura medida pelo próprio menu (a estimativa só vale até ele avisar).
            let mh = c.menu_heights.get(&m.panel).copied().unwrap_or(m.height);
            let mx = (x + tile_w - MENU_W - 8.0).max(4.0);
            let bottom = (vy + vh - MENU_GAP).max(top + 4.0 + mh);
            if let Some(ov) = &ov {
                set_if_changed!(ov, get_menu_x, set_menu_x, mx);
                set_if_changed!(ov, get_menu_bottom, set_menu_bottom, bottom);
                m.apply(ov);
            }
            region.push((px(mx), px(bottom - mh), px(MENU_W), px(mh), px(MENU_RADIUS)));
        }
        session::player_rect(
            v.id.clone(),
            px(vx),
            px(vy),
            px(vw),
            px(vh),
            players_visible && v.watching,
            if fs { 0 } else { px(VIDEO_RADIUS) },
            !chrome,
        );

        let buffer = num("buffer_ms") as u32;
        tiles.push(Tile {
            id: s(&v.id),
            gen_tag: s(gen_tag(num("fps"), num("shown_fps"))),
            name: s(if v.host_name.is_empty() { "Conectando…" } else { &v.host_name }),
            initials: s(initials(if v.host_name.is_empty() { "?" } else { &v.host_name })),
            watching: v.watching,
            step: v.step,
            busy: v.busy,
            res: s(if stats.is_some() {
                let (fps, shown) = (num("fps").round() as u32, num("shown_fps").round() as u32);
                // fps escolhido para assistir: "30→120fps" (recebidos → mostrados).
                if shown.abs_diff(fps) > 2 { format!("{}p {}→{}fps", num("height") as u32, fps, shown) } else { format!("{}p {}fps", num("height") as u32, fps) }
            } else {
                String::new()
            }),
            rate: s(format_rate(num("kbps"))),
            ping: s(v.ping.map(|p| if buffer > 0 { format!("{p} ms +{buffer}") } else { format!("{p} ms") }).unwrap_or_default()),
            ping_warn: v.ping.is_some_and(|p| p > 150),
            volume: v.volume as i32,
            muted: v.muted,
            chrome,
            focused: c.focus.as_deref() == Some(v.id.as_str()),
            x,
            y,
            w: tile_w,
            h: tile_h,
        });
    }
    crate::player::end_batch();
    let menu_id = s(if menu_tile { c.menu.clone().unwrap_or_default() } else { String::new() });
    set_if_changed!(a, get_menu_tile, set_menu_tile, menu_id.clone());
    if let Some(ov) = &ov {
        set_if_changed!(ov, get_menu_tile, set_menu_tile, menu_id);
        set_if_changed!(ov, get_top, set_top, top);
        set_if_changed!(ov, get_fullscreen, set_fullscreen, fs);
        sync(ov.get_tiles(), tiles.clone(), |m| ov.set_tiles(m));
    }
    overlay::set_region(if players_visible { &region } else { &[] });
    sync(a.get_tiles(), tiles, |m| a.set_tiles(m));
    set_if_changed!(a, get_has_focus, set_has_focus, c.focus.is_some());
}

/// Multiplicador da geração de quadros que está acontecendo ("2x"), pelo fps recebido e
/// o mostrado.
fn gen_tag(fps: f64, shown: f64) -> String {
    if fps > 1.0 && shown > fps * 1.4 { format!("{}x", (shown / fps).round() as u32) } else { String::new() }
}

/// Conteúdo do menu de engrenagem de uma stream.
struct GearMenu {
    height: f32,
    panel: String,
    fps: Vec<MenuOption>,
    quality: Vec<MenuOption>,
    fps_label: String,
    quality_label: String,
    gen_tag: String,
    info: String,
    fps_hint: String,
}

impl GearMenu {
    fn apply(&self, a: &Ov) {
        set_if_changed!(a, get_menu_panel, set_menu_panel, s(&self.panel));
        sync(a.get_menu_fps(), self.fps.clone(), |m| a.set_menu_fps(m));
        sync(a.get_menu_quality(), self.quality.clone(), |m| a.set_menu_quality(m));
        set_if_changed!(a, get_menu_fps_label, set_menu_fps_label, s(&self.fps_label));
        set_if_changed!(a, get_menu_quality_label, set_menu_quality_label, s(&self.quality_label));
        set_if_changed!(a, get_menu_gen_tag, set_menu_gen_tag, s(&self.gen_tag));
        set_if_changed!(a, get_menu_info, set_menu_info, s(&self.info));
        set_if_changed!(a, get_menu_fps_hint, set_menu_fps_hint, s(&self.fps_hint));
    }
}

fn option(value: &str, label: String, tag: String, note: &str, selected: bool) -> MenuOption {
    MenuOption { value: s(value), label: s(label), tag: s(tag), note: s(note), selected }
}

fn gear_menu(c: &Ctl, ui: &AppWindow, v: &Viewer) -> GearMenu {
    let num = |k: &str| v.stats.as_ref().and_then(|st| st[k].as_f64()).unwrap_or(0.0);
    let stream = v.nominal_fps;
    let height = num("height") as u32;
    let hz = monitor_hz(ui);
    let chosen = c.s.view_fps;
    // fps para assistir: acima do da stream, a placa gera quadros ("FG 2x"); abaixo, descarta.
    let mut fps = vec![];
    for (value, f) in [("0", 0u32), ("30", 30), ("60", 60), ("120", 120), ("144", 144)] {
        if f > hz || (f > stream.max(30) && !c.framegen_available) {
            continue;
        }
        let label = match (f, stream) {
            (0, 0) => "Original".to_string(),
            (0, st) => format!("Original ({st})"),
            (f, _) => format!("{f} fps"),
        };
        // As mesmas contas do framegen::Pacer: gera com 2x ou mais, descarta abaixo de
        // ~87% do fps da stream; perto dele mostra os quadros como vêm (60 numa de 57).
        let mult = if f > 0 && stream > 0 { (f as f64 / stream as f64).round() as u32 } else { 0 };
        let tag = if mult >= 2 { format!("FG {}x", mult.min(8)) } else { String::new() };
        let note = if f > 0 && stream > 0 && (f as f64) * 1.15 < stream as f64 { "descarta quadros" } else { "" };
        fps.push(option(value, label, tag, note, chosen == f));
    }
    let gen = fps.iter().find(|o| o.selected).map(|o| o.tag.to_string().replace("FG ", "")).unwrap_or_default();
    // Qualidade: Auto ou uma das versões que o host oferece.
    let mut quality = vec![option("auto", "Auto".into(), String::new(), &if height > 0 { format!("{height}p agora") } else { String::new() }, v.quality == "auto")];
    for h in &v.renditions {
        quality.push(option(&h.to_string(), format!("{h}p"), String::new(), if *h <= 480 { "economiza internet" } else { "" }, v.quality == h.to_string()));
    }
    if v.renditions.is_empty() && height > 0 {
        quality.push(option(&height.to_string(), format!("{height}p"), String::new(), "a única que o host manda", v.quality == height.to_string()));
    }
    let rows = |n: usize| 12.0 + 40.0 + 9.0 + n as f32 * 42.0 + 9.0;
    let panel = c.menu_panel.clone();
    let menu_height = match panel.as_str() {
        "fps" => rows(fps.len()) + 48.0,
        "quality" => rows(quality.len()) + 36.0,
        _ => 12.0 + 44.0 * 2.0 + 4.0 + 9.0 + 2.0 + 32.0,
    };
    GearMenu {
        height: menu_height,
        panel,
        fps_label: if chosen == 0 { "Original".into() } else { chosen.to_string() },
        quality_label: match (v.quality.as_str(), height) {
            ("auto", 0) => "Auto".into(),
            ("auto", h) => format!("Auto · {h}p"),
            (q, _) => format!("{q}p"),
        },
        gen_tag: gen,
        info: if height > 0 { format!("Stream: {height}p a {stream} fps") } else { "Conectando…".into() },
        fps_hint: if c.framegen_available {
            "FG = quadros gerados pela placa NVIDIA (2x = um gerado para cada quadro real). Soma ~16–26 ms de atraso.".into()
        } else {
            "Acima do fps da stream precisa de placa NVIDIA (GTX 16 ou RTX).".into()
        },
        fps,
        quality,
    }
}

fn toggle_menu(c: &mut Ctl, ui: &AppWindow, id: &str) {
    c.menu = if c.menu.as_deref() == Some(id) { None } else { Some(id.to_string()) };
    c.menu_panel = "main".into();
    relayout(c, ui);
}

fn close_menu(c: &mut Ctl, ui: &AppWindow) {
    if c.menu.take().is_some() {
        relayout(c, ui);
    }
}

fn render_all(c: &mut Ctl, ui: &AppWindow) {
    render_general(c, ui);
    render_settings(c, ui);
    render_public(c, ui);
    render_watching_list(c, ui);
    render_picker(c, ui);
    render_host_code(c, ui);
    render_toasts(c, ui);
    relayout(c, ui);
}

/* ============================================================== ações */

fn set_view(c: &mut Ctl, ui: &AppWindow, view: &str) {
    c.view = view.to_string();
    render_general(c, ui);
    relayout(c, ui);
    if view == "home" {
        refresh_public(c, ui);
    }
}

fn set_fullscreen(c: &mut Ctl, ui: &AppWindow, on: bool) {
    if c.fullscreen == on {
        return;
    }
    c.fullscreen = on;
    // O vídeo muda de tamanho: quem estava com os controles à mostra continua com eles.
    if let Some((_, until)) = &mut c.hover {
        *until = Instant::now() + CONTROLS_TIME;
    }
    // A interface já no modo novo (sem a barra de cima, por exemplo) antes de a janela
    // mudar de tamanho, para o primeiro quadro no tamanho novo já sair certo.
    render_general(c, ui);
    match hwnd_of(ui) {
        Some(h) => {
            crate::fullscreen::set(h, on);
            if !on {
                paint_titlebar(h, ui.global::<Theme>().get_dark());
            }
        }
        None => ui.window().set_fullscreen(on),
    }
    relayout(c, ui);
}

/// Tela cheia só com a stream `id`; se já estiver em tela cheia, volta para a grade.
fn toggle_stream_fullscreen(c: &mut Ctl, ui: &AppWindow, id: &str) {
    if c.fullscreen {
        c.focus = None;
        set_fullscreen(c, ui, false);
    } else {
        c.focus = Some(id.to_string());
        set_fullscreen(c, ui, true);
    }
    relayout(c, ui);
}

/// Controles por cima do vídeo: (stream com eles à mostra, stream cuja janela de
/// controles ainda tem região, que dura até o fim do esmaecer).
fn controls_state(c: &Ctl, now: Instant) -> (Option<String>, Option<String>) {
    match &c.hover {
        Some((id, until)) => ((*until > now).then(|| id.clone()), (*until + CONTROLS_FADE > now).then(|| id.clone())),
        None => (None, None),
    }
}

/// O mouse mexeu sobre o vídeo ou os controles da stream `id`: mostra os controles dela
/// (só dela) por mais um tempo.
fn wake_controls(c: &mut Ctl, ui: &AppWindow, id: &str) {
    let now = Instant::now();
    let was_on = controls_state(c, now).0.as_deref() == Some(id);
    c.hover = Some((id.to_string(), now + CONTROLS_TIME));
    if !was_on {
        c.last_controls = controls_state(c, now);
        relayout(c, ui);
    }
}

/// O mouse saiu do vídeo: os controles somem logo (se ele foi para os controles, eles
/// avisam e continuam).
fn leave_controls(c: &mut Ctl, id: &str) {
    if let Some((h, until)) = &mut c.hover {
        if h == id {
            *until = (*until).min(Instant::now() + CONTROLS_LEAVE);
        }
    }
}

/// Cria a janela dos controles por cima do vídeo, com a principal como dona.
fn create_overlay(c: &mut Ctl, ui: &AppWindow, main: isize) {
    overlay::prepare(main);
    let created = VideoOverlay::new();
    overlay::prepare_done();
    match created {
        Ok(o) => {
            o.global::<Theme>().set_dark(ui.global::<Theme>().get_dark());
            o.global::<Ov>().set_debug_tint(std::env::var_os("AJ_OVERLAY_DEBUG").is_some());
            wire_overlay(&o);
            if let Err(e) = o.show() {
                eprintln!("controles sobre o vídeo: {e}");
            }
            c.overlay = Some(o);
        }
        Err(e) => eprintln!("controles sobre o vídeo: {e}"),
    }
}

fn refresh_public(c: &mut Ctl, ui: &AppWindow) {
    if c.public_busy || c.net != "ok" {
        return;
    }
    c.public_busy = true;
    render_public(c, ui);
    let app = c.app.clone();
    bg(move || rendezvous::public_list(app.rdv()), |c, ui, r| {
        c.public_busy = false;
        match r {
            Ok(mut list) => {
                // O que chegou pela fofoca vale sobre o mural (mais novo, ou já encerrado).
                c.gossip.retain(|_, e| rendezvous::entry_alive(e));
                for e in c.gossip.values() {
                    rendezvous::merge_public(&mut list, e);
                }
                c.public = Some(list);
                c.public_updated = Some(Instant::now());
            }
            Err(_) => {
                if c.public.is_none() {
                    c.public = Some(Vec::new());
                }
            }
        }
        render_public(c, ui);
    });
}

fn refresh_apps(_c: &mut Ctl, _ui: &AppWindow) {
    bg(audio::audio_apps, |c, ui, r| {
        c.apps = r.unwrap_or_default();
        let discord = c.apps.iter().find(|a| a.exe.starts_with("discord")).map(|a| a.exe.clone());
        let mut changed = false;
        // Primeira vez: se o Discord estiver aberto, já tira ele da transmissão.
        if c.s.audio_mode.is_empty() {
            c.s.audio_mode = if discord.is_some() { "exclude".into() } else { "system".into() };
            c.s.audio_app = discord.unwrap_or_default();
            changed = true;
        } else if c.s.audio_app.is_empty() {
            if let Some(d) = discord {
                c.s.audio_app = d;
                changed = true;
            }
        }
        if changed {
            settings::save(&c.s);
            push_config(c, ui);
        }
        render_settings(c, ui);
    });
}

fn host_config(c: &Ctl) -> Option<session::HostConfig> {
    let src = c.source.as_ref()?;
    Some(session::HostConfig {
        source: src.id.clone(),
        name: my_name(c),
        res: c.s.res,
        fps: c.s.fps,
        bitrate: c.s.bitrate,
        cursor: c.s.cursor,
        audio_mode: if c.s.audio_mode.is_empty() { "none".into() } else { c.s.audio_mode.clone() },
        audio_app: c.s.audio_app.clone(),
        abitrate: c.s.abitrate,
        volume: c.s.volume,
        public: c.s.public,
        title: src.name.clone(),
        password: c.s.stream_password.clone(),
        mesh: c.s.mesh,
    })
}

fn my_name(c: &Ctl) -> String {
    let n = c.s.name.trim();
    if n.is_empty() { "Anônimo".into() } else { n.chars().take(40).collect() }
}

/// Grava as configurações daqui a pouco, uma vez só. Arrastar um controle (volume,
/// bitrate) chamava a gravação no disco a cada pixel.
fn save_soon(c: &mut Ctl) {
    if c.save_pending {
        return;
    }
    c.save_pending = true;
    slint::Timer::single_shot(Duration::from_millis(400), || {
        with(|c, _| {
            c.save_pending = false;
            settings::save(&c.s);
        });
    });
}

/// Manda a configuração ao host daqui a pouco, com o valor mais novo. Antes cada pixel
/// arrastado num controle abria uma thread que reconfigurava a transmissão.
fn push_soon(c: &mut Ctl) {
    if !c.live || c.push_pending {
        return;
    }
    c.push_pending = true;
    slint::Timer::single_shot(Duration::from_millis(150), || {
        with(|c, ui| {
            c.push_pending = false;
            push_config(c, ui);
        });
    });
}

fn push_config(c: &mut Ctl, _ui: &AppWindow) {
    if !c.live {
        return;
    }
    let Some(cfg) = host_config(c) else { return };
    let app = c.app.clone();
    bg(move || session::host_update(app, cfg), |c, ui, r| {
        if let Err(e) = r {
            toast(c, ui, "err", e);
        }
    });
}

fn set_setting(c: &mut Ctl, ui: &AppWindow, key: &str, value: &str) {
    let num = || value.parse::<u32>().ok();
    let on = value == "1";
    match key {
        "res" => c.s.res = num().unwrap_or(c.s.res),
        "fps" => c.s.fps = num().unwrap_or(c.s.fps),
        "bitrate" => c.s.bitrate = num().unwrap_or(c.s.bitrate),
        "abitrate" => c.s.abitrate = num().unwrap_or(c.s.abitrate),
        "volume" => c.s.volume = num().unwrap_or(c.s.volume),
        "cursor" => c.s.cursor = on,
        "mesh" => c.s.mesh = on,
        "password" => {
            let new = value.trim().to_string();
            if new == c.s.stream_password {
                return;
            }
            c.s.stream_password = new;
            settings::save(&c.s);
            render_settings(c, ui);
            push_config(c, ui);
            let msg = if c.s.stream_password.is_empty() { "Senha removida." } else { "Senha definida." };
            if c.live {
                // A senha entra nas chaves do código: gera outro (quem já assiste continua).
                let app = c.app.clone();
                bg(move || session::host_new_code(&app), move |c, ui, r| match r {
                    Ok(code) => {
                        c.code = code;
                        render_host_code(c, ui);
                        toast(c, ui, "ok", format!("{msg} Código novo gerado; quem já está assistindo continua."));
                    }
                    Err(e) => toast(c, ui, "err", e),
                });
            } else {
                toast(c, ui, "ok", msg);
            }
            return;
        }
        "public" => c.s.public = on,
        "audio-app" => c.s.audio_app = value.to_string(),
        "audio-mode" => {
            c.s.audio_mode = value.to_string();
            if (value == "exclude" || value == "include") && c.s.audio_app.is_empty() {
                c.s.audio_app = c.apps.first().map(|a| a.exe.clone()).unwrap_or_default();
            }
        }
        _ => return,
    }
    save_soon(c);
    render_settings(c, ui);
    push_soon(c);
}

fn open_picker(c: &mut Ctl, ui: &AppWindow, mode: &str) {
    // O seletor abre fora da grade: lá os vídeos nativos o cobririam.
    if c.view == "watch" {
        c.view = if c.live { "host".into() } else { "home".into() };
    }
    c.picker = mode.to_string();
    c.picker_selected.clear();
    if mode == "start" {
        // Cada transmissão nova começa no padrão: aparece em "Ao vivo agora", sem senha.
        c.s.public = true;
        c.s.stream_password.clear();
        settings::save(&c.s);
        render_settings(c, ui);
        let a = ui.global::<App>();
        set_if_changed!(a, get_picker_pw_on, set_picker_pw_on, false);
        set_if_changed!(a, get_picker_pw, set_picker_pw, s(""));
    }
    render_general(c, ui);
    relayout(c, ui);
    load_sources(c, ui);
    refresh_apps(c, ui);
}

fn load_sources(c: &mut Ctl, ui: &AppWindow) {
    c.sources = None;
    render_picker(c, ui);
    bg(
        || {
            let list = session::sources_list();
            let thumbs: Vec<(String, SharedPixelBuffer<Rgba8Pixel>)> =
                list.iter().filter_map(|x| decode_data_url(&x.thumbnail).map(|b| (x.id.clone(), b))).collect();
            (list, thumbs)
        },
        |c, ui, (list, thumbs)| {
            c.thumbs = thumbs.into_iter().map(|(id, buf)| (id, Image::from_rgba8(buf))).collect();
            c.sources = Some(list);
            render_picker(c, ui);
        },
    );
}

fn close_picker(c: &mut Ctl, ui: &AppWindow) {
    c.picker.clear();
    c.picker_selected.clear();
    render_picker(c, ui);
    relayout(c, ui);
}

fn picker_confirm(c: &mut Ctl, ui: &AppWindow, id: &str) {
    let Some(src) = c.sources.as_ref().and_then(|l| l.iter().find(|x| x.id == id)).cloned() else { return };
    let mode = c.picker.clone();
    if mode == "start" {
        // A senha escolhida nesta tela vale para a transmissão que vai começar.
        let a = ui.global::<App>();
        let password = if a.get_picker_pw_on() { a.get_picker_pw().trim().to_string() } else { String::new() };
        if a.get_picker_pw_on() && password.is_empty() {
            return toast(c, ui, "err", "Digite a senha ou desligue “Pedir senha”.");
        }
        c.s.stream_password = password;
        settings::save(&c.s);
        render_settings(c, ui);
    }
    close_picker(c, ui);
    let preview = c.thumbs.get(&src.id).cloned();
    if mode == "switch" {
        let name = src.name.clone();
        c.source = Some(src);
        if let Some(p) = preview {
            ui.global::<App>().set_preview(p);
            ui.global::<App>().set_has_preview(true);
        }
        render_host_code(c, ui);
        push_config(c, ui);
        toast(c, ui, "ok", format!("Agora transmitindo: {name}"));
    } else {
        start_sharing(c, ui, src, preview);
    }
}

fn start_sharing(c: &mut Ctl, ui: &AppWindow, src: capture::Source, preview: Option<Image>) {
    c.source = Some(src);
    c.live = true;
    c.code.clear();
    c.host_viewers = 0;
    c.view = "host".into();
    let a = ui.global::<App>();
    set_if_changed!(a, get_has_preview, set_has_preview, preview.is_some());
    if let Some(p) = preview {
        set_if_changed!(a, get_preview, set_preview, p);
    }
    set_if_changed!(a, get_has_info, set_has_info, false);
    set_if_changed!(a, get_peers, set_peers, model(Vec::new()));
    render_general(c, ui);
    render_host_code(c, ui);
    relayout(c, ui);
    let Some(cfg) = host_config(c) else { return };
    let app = c.app.clone();
    bg(move || session::host_start(app, cfg), |c, ui, r| match r {
        Ok(code) => {
            c.code = code;
            render_host_code(c, ui);
        }
        Err(e) => {
            c.live = false;
            c.view = "home".into();
            render_general(c, ui);
            toast(c, ui, "err", e);
        }
    });
}

fn stop_sharing(c: &mut Ctl, ui: &AppWindow, reason: Option<&str>) {
    if !c.live {
        return;
    }
    session::host_stop(&c.app);
    c.live = false;
    c.code.clear();
    c.host_viewers = 0;
    if c.view == "host" {
        c.view = "home".into();
    }
    ui.global::<App>().set_has_preview(false);
    ui.global::<App>().set_has_info(false);
    ui.global::<App>().set_stat_mesh(s(""));
    render_general(c, ui);
    render_host_code(c, ui);
    if let Some(r) = reason {
        toast(c, ui, "", r);
    }
}

fn join_with_code(c: &mut Ctl, ui: &AppWindow, raw: &str) {
    let code: String = raw.chars().filter(|ch| !ch.is_whitespace()).collect();
    if code.is_empty() {
        return toast(c, ui, "err", "Cole o código da transmissão primeiro.");
    }
    let locked = match rendezvous::check_code(&code) {
        Ok(l) => l,
        Err(e) => return toast(c, ui, "err", e),
    };
    if c.live && c.code == code {
        return toast(c, ui, "err", "Esse é o código da sua própria transmissão.");
    }
    if c.viewers.iter().any(|v| v.code == code) {
        set_view(c, ui, "watch");
        return toast(c, ui, "", "Você já está assistindo essa transmissão.");
    }
    if locked {
        // Pede a senha antes (os vídeos nativos cobririam a janela: ficam escondidos).
        c.password_for = Some(code);
        let a = ui.global::<App>();
        set_if_changed!(a, get_password_error, set_password_error, s(""));
        set_if_changed!(a, get_password_prompt, set_password_prompt, true);
        relayout(c, ui);
        return;
    }
    start_watching(c, ui, code, None);
}

fn submit_password(c: &mut Ctl, ui: &AppWindow, password: &str) {
    let Some(code) = c.password_for.clone() else { return };
    if password.is_empty() {
        ui.global::<App>().set_password_error(s("Digite a senha."));
        return;
    }
    c.password_for = None;
    ui.global::<App>().set_password_prompt(false);
    start_watching(c, ui, code, Some(password.to_string()));
}

fn start_watching(c: &mut Ctl, ui: &AppWindow, code: String, password: Option<String>) {
    let id = random_id();
    let volume = c.s.view_volume.min(100);
    c.viewers.push(Viewer {
        id: id.clone(),
        code: code.clone(),
        host_name: String::new(),
        step: 0,
        busy: false,
        watching: false,
        stats: None,
        ping: None,
        volume,
        muted: false,
        quality: "auto".into(),
        renditions: vec![],
        nominal_fps: 0,
        fps_candidate: (0, 0),
    });
    c.focus = None;
    c.view = "watch".into();
    render_general(c, ui);
    render_watching_list(c, ui);
    render_public(c, ui);
    relayout(c, ui);

    let (app, name) = (c.app.clone(), my_name(c));
    let view_fps = view_fps(c, ui);
    transport::tokio_rt().spawn(async move {
        let r = session::viewer_join(app, id.clone(), code, password, name, volume, view_fps).await;
        let _ = slint::invoke_from_event_loop(move || {
            with(|c, ui| match r {
                Ok(host) => {
                    if let Some(v) = c.viewers.iter_mut().find(|v| v.id == id) {
                        v.host_name = host;
                    }
                    render_watching_list(c, ui);
                    relayout(c, ui);
                }
                Err(e) if e == "cancelado" => {}
                Err(e) => end_watching(c, ui, &id, Some(&e), true),
            });
        });
    });
}

/// fps para assistir que vale de verdade: o escolhido, sem passar do que o monitor
/// mostra. (Sem placa NVIDIA compatível, o gerador desliga sozinho e só a redução vale.)
fn view_fps(c: &Ctl, ui: &AppWindow) -> u32 {
    match c.s.view_fps {
        0 => 0,
        f => f.min(monitor_hz(ui)),
    }
}

/// Taxa de atualização (Hz) do monitor onde está a janela principal.
fn monitor_hz(ui: &AppWindow) -> u32 {
    // Consultar o Windows custa alguns ms e o menu pede isso a cada atualização da tela:
    // guarda por 2 s (o suficiente para notar a janela indo para outro monitor).
    thread_local! {
        static CACHE: std::cell::Cell<Option<(u32, Instant)>> = const { std::cell::Cell::new(None) };
    }
    if let Some((hz, at)) = CACHE.get() {
        if at.elapsed() < Duration::from_secs(2) {
            return hz;
        }
    }
    let hz = query_monitor_hz(ui);
    CACHE.set(Some((hz, Instant::now())));
    hz
}

fn query_monitor_hz(ui: &AppWindow) -> u32 {
    use windows::Win32::Graphics::Gdi::*;
    let fallback = 60;
    let Some(hwnd) = hwnd_of(ui) else { return fallback };
    unsafe {
        let mon = MonitorFromWindow(windows::Win32::Foundation::HWND(hwnd as *mut _), MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(mon, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            return fallback;
        }
        let mut mode = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        if !EnumDisplaySettingsW(windows::core::PCWSTR(info.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode).as_bool() {
            return fallback;
        }
        match mode.dmDisplayFrequency {
            f if f >= 24 => f,
            _ => fallback,
        }
    }
}

fn end_watching(c: &mut Ctl, ui: &AppWindow, id: &str, message: Option<&str>, is_error: bool) {
    if c.menu.as_deref() == Some(id) {
        c.menu = None;
    }
    let Some(pos) = c.viewers.iter().position(|v| v.id == id) else { return };
    let v = c.viewers.remove(pos);
    if c.focus.as_deref() == Some(id) {
        c.focus = None;
        set_fullscreen(c, ui, false);
    }
    let app = c.app.clone();
    let vid = v.id.clone();
    transport::tokio_rt().spawn(async move { session::viewer_leave(app, vid).await });
    render_general(c, ui);
    render_watching_list(c, ui);
    render_public(c, ui);
    relayout(c, ui);
    if let Some(m) = message {
        let text = if v.host_name.is_empty() { m.to_string() } else { format!("{}: {m}", v.host_name) };
        toast(c, ui, if is_error { "err" } else { "" }, text);
    }
}

fn set_viewer_volume(c: &mut Ctl, ui: &AppWindow, id: &str, volume: u32, muted: bool) {
    if let Some(v) = c.viewers.iter_mut().find(|v| v.id == id) {
        v.volume = volume;
        v.muted = muted;
        session::viewer_volume(id.to_string(), volume, muted);
    }
    c.s.view_volume = volume;
    save_soon(c);
    relayout(c, ui);
}

/* ======================================================= eventos do motor */

fn on_event(name: &str, value: Value) {
    with(|c, ui| handle_event(c, ui, name, value));
}

fn handle_event(c: &mut Ctl, ui: &AppWindow, name: &str, v: Value) {
    let a = ui.global::<App>();
    let id_of = |v: &Value| v["id"].as_str().unwrap_or_default().to_string();
    match name {
        // Anúncio do "Ao vivo agora" pela fofoca: entra na lista na hora.
        "public-entry" => {
            let Ok(e) = serde_json::from_value::<rendezvous::PublicEntry>(v) else { return };
            if c.gossip.get(&e.code).is_some_and(|old| old.ts >= e.ts) {
                return;
            }
            c.gossip.insert(e.code.clone(), e.clone());
            let list = c.public.get_or_insert_with(Vec::new);
            if rendezvous::merge_public(list, &e) {
                render_public(c, ui);
            }
        }
        "dht-status" => {
            c.net = if v.as_bool().unwrap_or(false) { "ok".into() } else { "err".into() };
            render_general(c, ui);
            render_public(c, ui);
            refresh_public(c, ui);
        }
        "host-peers" => {
            let list = v.as_array().cloned().unwrap_or_default();
            c.host_viewers = list.iter().filter(|p| p["state"] != "connecting").count() as u32;
            let peers: Vec<Peer> = list
                .iter()
                .map(|p| {
                    let name = p["name"].as_str().unwrap_or("Espectador");
                    let state = p["state"].as_str().unwrap_or("");
                    Peer {
                        id: p["id"].as_i64().unwrap_or(0) as i32,
                        name: s(name),
                        initials: s(initials(name)),
                        state_text: s(match state {
                            "connected" => "Assistindo agora",
                            "unstable" => "Conexão instável",
                            _ => "Entrando",
                        }),
                        connected: state == "connected",
                    }
                })
                .collect();
            sync(a.get_peers(), peers, |m| a.set_peers(m));
            render_general(c, ui);
        }
        "host-mesh" => {
            let n = v["members"].as_u64().unwrap_or(0);
            let pct = v["percent"].as_f64().unwrap_or(100.0).round() as u32;
            let text = match n {
                0 => String::new(),
                1 => "Malha: 1 assistindo".to_string(),
                n => format!("Malha: {n} · envio {pct}%"),
            };
            set_if_changed!(a, get_stat_mesh, set_stat_mesh, s(text));
        }
        "host-info" => {
            let f = |k: &str| v[k].as_f64().unwrap_or(0.0);
            let hw = v["hardware"].as_bool().unwrap_or(false);
            set_if_changed!(a, get_has_info, set_has_info, true);
            set_if_changed!(a, get_hardware, set_hardware, hw);
            set_if_changed!(a, get_stat_res, set_stat_res, s(format!("{}×{}", f("width") as u32, f("height") as u32)));
            set_if_changed!(a, get_stat_fps, set_stat_fps, s((f("fps").round() as u32).to_string()));
            set_if_changed!(a, get_stat_rate, set_stat_rate, s(format!("{} de {}", format_rate(f("kbps")), format_rate(f("target_kbps")))));
            // Bitrate adaptativo: a rede de alguém que assiste não aguenta o escolhido.
            let (net_kbps, net_fps) = (f("net_kbps"), f("net_fps") as u32);
            a.set_stat_net(s(match (net_kbps > 0.0, net_fps > 0) {
                (false, false) => String::new(),
                (true, false) => format!("Rede: até {}", format_rate(net_kbps)),
                (false, true) => format!("Rede: {net_fps} fps"),
                (true, true) => format!("Rede: até {}, {net_fps} fps", format_rate(net_kbps)),
            }));
            a.set_encoder_line(s(format!(
                "H.264 com {}{}",
                v["encoder"].as_str().unwrap_or("?"),
                if hw { ", na placa de vídeo" } else { ", no processador" }
            )));
            a.set_encoder_detail(s(format!(
                "{} {} ms por quadro. O Windows entregou {} quadros novos por segundo; o resto são repetições de imagem parada.",
                if v["zero_copy"].as_bool().unwrap_or(false) { "A imagem não sai da placa de vídeo (sem cópia para a CPU)." } else { "A imagem passa pela CPU antes de codificar." },
                format!("{:.1}", f("encode_ms")).replace('.', ","),
                f("captured_fps").round() as u32
            )));
        }
        "host-preview" => {
            if let Some(buf) = v.as_str().and_then(decode_data_url) {
                set_if_changed!(a, get_preview, set_preview, Image::from_rgba8(buf));
                set_if_changed!(a, get_has_preview, set_has_preview, true);
            }
        }
        "host-audio-level" => a.set_audio_level(v.as_f64().unwrap_or(0.0) as f32),
        "host-audio" => {
            let status = v["status"].as_str().unwrap_or("");
            let detail = v["detail"].as_str().unwrap_or("");
            let (text, kind) = match status {
                "capturing" => {
                    let base = match c.s.audio_mode.as_str() {
                        "system" => "Capturando todo o áudio do PC".to_string(),
                        "exclude" => format!("Capturando tudo, menos {}", app_name(c, &c.s.audio_app)),
                        _ => format!("Capturando só {}", app_name(c, &c.s.audio_app)),
                    };
                    (if detail.is_empty() { base } else { format!("{base} ({detail})") }, "ok")
                }
                "waiting" => (format!("Esperando {detail} abrir"), ""),
                "error" => (format!("Erro no áudio: {detail}"), "err"),
                _ => {
                    set_if_changed!(a, get_audio_level, set_audio_level, 0.0);
                    ("Sem áudio".to_string(), "")
                }
            };
            set_if_changed!(a, get_audio_status, set_audio_status, s(text));
            set_if_changed!(a, get_audio_status_kind, set_audio_status_kind, s(kind));
        }
        "host-error" => toast(c, ui, "err", v.as_str().unwrap_or("Erro na transmissão")),
        "host-source-closed" => {
            toast(c, ui, "", "A janela transmitida foi fechada. Escolha outra fonte.");
            open_picker(c, ui, "switch");
        }
        "host-viewer-joined" => toast(c, ui, "ok", format!("{} está assistindo.", v.as_str().unwrap_or("Alguém"))),
        "host-viewer-left" => toast(c, ui, "", format!("{} saiu da transmissão.", v.as_str().unwrap_or("Alguém"))),

        "viewer-step" => {
            let id = id_of(&v);
            let value = v["value"].as_str().unwrap_or("");
            let step = match value {
                "publish" => 1,
                "wait" | "busy" => 2,
                "connect" => 3,
                _ => 0,
            };
            if let Some(x) = c.viewers.iter_mut().find(|x| x.id == id && !x.watching) {
                x.step = step;
                x.busy = value == "busy";
            }
            render_watching_list(c, ui);
            relayout(c, ui);
        }
        "viewer-state" => {
            let id = id_of(&v);
            let Some(x) = c.viewers.iter_mut().find(|x| x.id == id) else { return };
            match v["value"].as_str().unwrap_or("") {
                "connected" => {
                    x.watching = true;
                    render_watching_list(c, ui);
                    // Mostra os controles por um momento, para quem acabou de entrar ver
                    // que eles existem.
                    wake_controls(c, ui, &id);
                    relayout(c, ui);
                }
                "failed" => {
                    let msg = if x.watching { "A conexão caiu." } else { "Não deu para conectar direto. Alguns roteadores bloqueiam conexões P2P." };
                    end_watching(c, ui, &id, Some(msg), true);
                }
                "closed" => end_watching(c, ui, &id, Some("A transmissão terminou."), false),
                _ => {}
            }
        }
        "viewer-ended" => {
            let id = id_of(&v);
            let reason = v["value"].as_str().unwrap_or("O host encerrou a transmissão.").to_string();
            end_watching(c, ui, &id, Some(&reason), false);
        }
        "viewer-stats" => {
            let id = id_of(&v);
            #[cfg(debug_assertions)]
            if std::env::var_os("AJ_AUTOTEST").is_some() {
                eprintln!("[stats] {v}");
            }
            if let Some(x) = c.viewers.iter_mut().find(|x| x.id == id) {
                x.update_nominal_fps(v["fps"].as_f64().unwrap_or(0.0));
                x.stats = Some(v);
                relayout(c, ui);
            }
        }
        "viewer-renditions" => {
            let id = id_of(&v);
            if let Some(x) = c.viewers.iter_mut().find(|x| x.id == id) {
                x.renditions = v["value"].as_array().map(|a| a.iter().filter_map(|h| h.as_u64().map(|h| h as u32)).collect()).unwrap_or_default();
                relayout(c, ui);
            }
        }
        "viewer-ping" => {
            let id = id_of(&v);
            if let Some(x) = c.viewers.iter_mut().find(|x| x.id == id) {
                x.ping = v["value"].as_u64().map(|p| p as u32);
                render_watching_list(c, ui);
                relayout(c, ui);
            }
        }
        "player-input" => {
            let id = id_of(&v);
            match v["kind"].as_str().unwrap_or("") {
                "dblclick" => toggle_stream_fullscreen(c, ui, &id),
                "down" => close_menu(c, ui),
                "move" => wake_controls(c, ui, &id),
                "leave" => leave_controls(c, &id),
                "f11" => set_fullscreen(c, ui, !c.fullscreen),
                "escape" => set_fullscreen(c, ui, false),
                _ => {}
            }
        }
        _ => {}
    }
}

/* ================================================================ início */

fn wire(ui: &AppWindow) {
    let a = ui.global::<App>();
    a.on_navigate(|view| later(move |c, ui| set_view(c, ui, &view)));
    a.on_toggle_theme(|| {
        later(|c, ui| {
            let dark = !ui.global::<Theme>().get_dark();
            // Vale só para esta sessão: o app sempre abre no tema do Windows.
            ui.global::<Theme>().set_dark(dark);
            if let Some(o) = &c.overlay {
                o.global::<Theme>().set_dark(dark);
            }
            if let Some(h) = hwnd_of(ui) {
                paint_titlebar(h, dark);
            }
        })
    });
    a.on_name_edited(|name| {
        later(move |c, _| {
            c.s.name = name.to_string();
            settings::save(&c.s);
        })
    });
    a.on_layout_changed(|| later(|c, ui| relayout(c, ui)));
    a.on_key_f11(|| later(|c, ui| set_fullscreen(c, ui, !c.fullscreen)));
    a.on_key_escape(|| {
        later(|c, ui| {
            if c.menu.is_some() {
                close_menu(c, ui);
            } else if !c.picker.is_empty() {
                close_picker(c, ui);
            } else {
                set_fullscreen(c, ui, false);
            }
        })
    });
    a.on_refresh_public(|| later(|c, ui| refresh_public(c, ui)));
    a.on_watch(|code| later(move |c, ui| join_with_code(c, ui, &code)));
    a.on_join(|code| later(move |c, ui| join_with_code(c, ui, &code)));
    a.on_paste_join(|| {
        later(|c, ui| match clipboard_get() {
            Some(t) if !t.is_empty() => join_with_code(c, ui, &t),
            _ => toast(c, ui, "err", "A área de transferência está vazia. Copie o código primeiro."),
        })
    });
    a.on_copy_code(|code| {
        later(move |c, ui| {
            if clipboard_set(&code) {
                toast(c, ui, "ok", "Código copiado. Agora é só enviar.");
            } else {
                toast(c, ui, "err", "Não deu para copiar o código.");
            }
        })
    });
    a.on_copy_my_code(|| {
        later(|c, ui| {
            let code = c.code.clone();
            if !code.is_empty() && clipboard_set(&code) {
                toast(c, ui, "ok", "Código copiado. Agora é só enviar.");
            }
        })
    });
    a.on_set_setting(|k, v| later(move |c, ui| set_setting(c, ui, &k, &v)));
    a.on_refresh_apps(|| later(|c, ui| refresh_apps(c, ui)));
    a.on_open_picker(|mode| later(move |c, ui| open_picker(c, ui, &mode)));
    a.on_close_picker(|| later(|c, ui| close_picker(c, ui)));
    a.on_picker_refresh(|| later(|c, ui| load_sources(c, ui)));
    a.on_picker_tab_changed(|tab| {
        later(move |c, ui| {
            c.picker_tab = tab.to_string();
            render_picker(c, ui);
        })
    });
    a.on_picker_select(|id| {
        later(move |c, ui| {
            c.picker_selected = id.to_string();
            ui.global::<App>().set_picker_selected(id);
        })
    });
    a.on_picker_confirm(|id| later(move |c, ui| picker_confirm(c, ui, &id)));
    a.on_picker_pw_toggled(|on| later(move |_, ui| ui.global::<App>().set_picker_pw_on(on)));
    a.on_picker_pw_edited(|t| later(move |_, ui| ui.global::<App>().set_picker_pw(t)));
    a.on_stop_sharing(|| later(|c, ui| stop_sharing(c, ui, None)));
    a.on_new_code(|| {
        later(|c, _| {
            let app = c.app.clone();
            bg(move || session::host_new_code(&app), |c, ui, r| match r {
                Ok(code) => {
                    c.code = code;
                    render_host_code(c, ui);
                    toast(c, ui, "ok", "Código novo gerado. O anterior parou de funcionar.");
                }
                Err(e) => toast(c, ui, "err", e),
            });
        })
    });
    a.on_kick(|id| later(move |c, _| session::host_kick(c.app.clone(), id.max(0) as u64)));
    a.on_leave(|id| later(move |c, ui| end_watching(c, ui, &id, None, false)));
    a.on_unfocus(|| {
        later(|c, ui| {
            c.focus = None;
            relayout(c, ui);
        })
    });
    a.on_submit_password(|p| later(move |c, ui| submit_password(c, ui, &p)));
    a.on_cancel_password(|| {
        later(|c, ui| {
            c.password_for = None;
            ui.global::<App>().set_password_prompt(false);
            relayout(c, ui);
        })
    });
    a.on_close_menu(|| later(|c, ui| close_menu(c, ui)));
}

/// Botões dos controles por cima do vídeo (janela própria, global `Ov`).
fn wire_overlay(o: &VideoOverlay) {
    let a = o.global::<Ov>();
    a.on_activity(|id| later(move |c, ui| wake_controls(c, ui, &id)));
    a.on_leave(|id| later(move |c, ui| end_watching(c, ui, &id, None, false)));
    a.on_close_menu(|| later(|c, ui| close_menu(c, ui)));
    a.on_toggle_focus(|id| {
        later(move |c, ui| {
            if c.focus.as_deref() == Some(id.as_str()) {
                c.focus = None;
                set_fullscreen(c, ui, false);
            } else {
                c.focus = Some(id.to_string());
            }
            relayout(c, ui);
        })
    });
    a.on_toggle_fullscreen(|id| later(move |c, ui| toggle_stream_fullscreen(c, ui, &id)));
    a.on_set_volume(|id, vol| later(move |c, ui| set_viewer_volume(c, ui, &id, vol.clamp(0, 100) as u32, false)));
    a.on_set_view_fps(|v| {
        later(move |c, ui| {
            c.s.view_fps = v.parse().unwrap_or(0);
            settings::save(&c.s);
            let fps = view_fps(c, ui);
            for x in &c.viewers {
                session::viewer_view_fps(&x.id, fps);
            }
            ui.global::<App>().set_view_fps(s(c.s.view_fps.to_string()));
            c.menu_panel = "main".into();
            relayout(c, ui);
            let hz = monitor_hz(ui);
            if c.s.view_fps > 0 && fps < c.s.view_fps {
                toast(c, ui, "", format!("Seu monitor mostra até {hz} quadros por segundo: assistindo a {fps} fps."));
            }
        })
    });
    a.on_toggle_menu(|id| {
        if trace() {
            eprintln!("[ui {:.0}] clique na engrenagem", tms());
        }
        later(move |c, ui| toggle_menu(c, ui, &id))
    });
    a.on_menu_resized(|h| {
        later(move |c, ui| {
            let h = h as f32;
            // Durante a montagem o menu pode medir só a borda; o menu real tem pelo menos
            // duas linhas (~90 px).
            if h >= 60.0 && c.menu_heights.get(&c.menu_panel).is_none_or(|old| (old - h).abs() > 0.5) {
                c.menu_heights.insert(c.menu_panel.clone(), h);
                relayout(c, ui);
            }
        })
    });
    a.on_menu_go(|panel| {
        later(move |c, ui| {
            c.menu_panel = panel.to_string();
            relayout(c, ui);
        })
    });
    a.on_pick_quality(|q| {
        later(move |c, ui| {
            let Some(id) = c.menu.clone() else { return };
            if let Some(v) = c.viewers.iter_mut().find(|v| v.id == id) {
                v.quality = q.to_string();
            }
            session::viewer_quality(&id, &q);
            c.menu_panel = "main".into();
            relayout(c, ui);
        })
    });
    a.on_toggle_mute(|id| {
        later(move |c, ui| {
            let Some((vol, muted)) = c.viewers.iter().find(|v| v.id == id.as_str()).map(|v| (v.volume, v.muted)) else { return };
            set_viewer_volume(c, ui, &id, vol, !muted);
        })
    });
}

fn start_updater() {
    if cfg!(debug_assertions) {
        return;
    }
    bg(updater::check, |c, ui, r| {
        let Ok(Some(update)) = r else { return };
        c.updating = true;
        ui.global::<App>().set_update_version(s(&update.version));
        ui.global::<App>().set_update_progress(-1.0);
        relayout(c, ui);
        std::thread::spawn(move || {
            let result = updater::download_and_install(&update, |done, total| {
                if let Some(t) = total.filter(|t| *t > 0) {
                    let p = done as f32 / t as f32;
                    let _ = slint::invoke_from_event_loop(move || {
                        with(|_, ui| ui.global::<App>().set_update_progress(p));
                    });
                }
            });
            let _ = slint::invoke_from_event_loop(move || match result {
                // O instalador assumiu: encerra para ele poder trocar o .exe.
                Ok(()) => {
                    with(|c, _| session::host_stop(&c.app));
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("atualização automática falhou: {e}");
                    with(|c, ui| {
                        c.updating = false;
                        ui.global::<App>().set_update_version(s(""));
                        relayout(c, ui);
                    });
                }
            });
        });
    });
}

pub fn run() -> Result<(), slint::PlatformError> {
    // A janela dos controles por cima do vídeo precisa de atributos do winit (dona, fora
    // da barra de tarefas) que o Slint não expõe.
    slint::BackendSelector::new().with_winit_window_attributes_hook(overlay::attributes_hook).select()?;
    let ui = AppWindow::new()?;

    let app = AppHandle::new(|name, value| {
        let name = name.to_string();
        let _ = slint::invoke_from_event_loop(move || on_event(&name, value));
    });

    let mut st = settings::load();
    if st.name.trim().is_empty() {
        st.name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Meu PC".into());
        settings::save(&st);
    }
    // Sempre abre no tema do Windows (o botão de tema vale para a sessão).
    let dark = settings::windows_prefers_dark();
    ui.global::<Theme>().set_dark(dark);
    ui.global::<App>().set_my_name(s(&st.name));

    CTL.with(|cell| {
        *cell.borrow_mut() = Some(Ctl {
            ui: ui.as_weak(),
            app: app.clone(),
            s: st,
            view: "home".into(),
            net: "pending".into(),
            live: false,
            source: None,
            code: String::new(),
            host_viewers: 0,
            apps: Vec::new(),
            public: None,
            public_busy: false,
            public_updated: None,
            gossip: HashMap::new(),
            sources: None,
            thumbs: HashMap::new(),
            picker: String::new(),
            picker_tab: "monitor".into(),
            picker_selected: String::new(),
            viewers: Vec::new(),
            focus: None,
            fullscreen: false,
            framegen_available: false,
            menu: None,
            menu_panel: "main".into(),
            password_for: None,
            hover: None,
            last_controls: (None, None),
            overlay: None,
            overlay_attached: false,
            toasts: Vec::new(),
            toast_seq: 0,
            updating: false,
            save_pending: false,
            push_pending: false,
            menu_heights: HashMap::new(),
        });
    });

    wire(&ui);
    with(|c, ui| render_all(c, ui));

    rendezvous::init(&app);
    if rendezvous::rdv_ready(app.rdv()) {
        with(|c, ui| {
            c.net = "ok".into();
            render_general(c, ui);
        });
    }

    #[cfg(debug_assertions)]
    autotest();

    // Ao fechar: encerra a transmissão e as conexões antes de sair.
    ui.window().on_close_requested(|| {
        with(|c, _| {
            if let Some(o) = &c.overlay {
                let _ = o.hide();
            }
            if c.save_pending {
                settings::save(&c.s);
            }
            session::host_stop(&c.app);
            for v in &c.viewers {
                let (app, id) = (c.app.clone(), v.id.clone());
                transport::tokio_rt().spawn(async move { session::viewer_leave(app, id).await });
            }
        });
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(400));
            std::process::exit(0);
        });
        slint::CloseRequestResponse::HideWindow
    });

    ui.show()?;
    if trace() {
        // Quanto cada quadro da interface leva para desenhar.
        let start = std::cell::Cell::new(None::<Instant>);
        let _ = ui.window().set_rendering_notifier(move |state, _| match state {
            slint::RenderingState::BeforeRendering => start.set(Some(Instant::now())),
            slint::RenderingState::AfterRendering => {
                if let Some(t) = start.take() {
                    eprintln!("[ui {:.0}] quadro desenhado em {:.1} ms", tms(), t.elapsed().as_secs_f64() * 1000.0);
                }
            }
            _ => {}
        });
    }

    // A janela nativa só existe depois que o loop começa: pega o HWND, pinta a barra e
    // cria a janela dos controles por cima do vídeo.
    let hwnd_timer = slint::Timer::default();
    {
        let app = app.clone();
        let weak = ui.as_weak();
        hwnd_timer.start(slint::TimerMode::Repeated, Duration::from_millis(30), move || {
            let Some(ui) = weak.upgrade() else { return };
            if app.hwnd() == 0 {
                if let Some(h) = hwnd_of(&ui) {
                    app.set_hwnd(h);
                    paint_titlebar(h, ui.global::<Theme>().get_dark());
                    with(|c, ui| {
                        create_overlay(c, ui, h);
                        relayout(c, ui);
                    });
                }
                return;
            }
            // Depois a dos controles (a janela nativa dela também nasce com o loop rodando).
            let main = app.hwnd();
            with(|c, ui| {
                if c.overlay_attached {
                    return;
                }
                if let Some(oh) = c.overlay.as_ref().and_then(|o| window_hwnd(o.window())) {
                    overlay::attach(main, oh);
                    c.overlay_attached = true;
                    relayout(c, ui);
                }
            });
        });
    }

    // Lista pública a cada 20 s (no Início); relógio dos textos "há X s"; avisos que expiram;
    // controles que somem em tela cheia.
    let public_timer = slint::Timer::default();
    public_timer.start(slint::TimerMode::Repeated, PUBLIC_REFRESH, || {
        with(|c, ui| {
            // Transmissões que pararam de se anunciar (o app fechou sem avisar) saem.
            if let Some(list) = &mut c.public {
                let before = list.len();
                list.retain(|p| rendezvous::entry_alive(&p.entry));
                if list.len() != before {
                    render_public(c, ui);
                }
            }
            // Com vizinhos na fofoca, os anúncios chegam sozinhos: a DHT só é relida de vez
            // em quando, para pegar quem ficou de fora da fofoca.
            let gossiping = crate::gossip::peer_count() > 0;
            let fresh = c.public_updated.is_some_and(|t| t.elapsed() < PUBLIC_REFRESH_GOSSIP);
            if c.view == "home" && !(gossiping && fresh) {
                refresh_public(c, ui);
            }
        });
    });
    let tick = slint::Timer::default();
    tick.start(slint::TimerMode::Repeated, Duration::from_millis(250), || {
        with(|c, ui| {
            let now = Instant::now();
            let before = c.toasts.len();
            c.toasts.retain(|t| t.3 > now);
            if c.toasts.len() != before {
                render_toasts(c, ui);
            }
            if c.hover.as_ref().is_some_and(|(_, until)| *until + CONTROLS_FADE <= now) {
                c.hover = None;
            }
            let state = controls_state(c, now);
            if state != c.last_controls {
                c.last_controls = state;
                relayout(c, ui);
            }
        });
    });
    let clock = slint::Timer::default();
    clock.start(slint::TimerMode::Repeated, Duration::from_secs(5), || {
        with(|c, ui| render_public(c, ui));
    });

    with(|c, ui| refresh_apps(c, ui));
    // A geração de quadros precisa de placa NVIDIA: descobre em segundo plano (inicia o CUDA).
    bg(session::framegen_available, |c, ui, ok| {
        c.framegen_available = ok;
        ui.global::<App>().set_framegen_available(ok);
        ui.global::<App>().set_view_fps(s(c.s.view_fps.to_string()));
    });
    start_updater();

    slint::run_event_loop_until_quit()
}

/// Teste automático (só na versão de desenvolvimento), para ver uma stream de verdade sem
/// ninguém clicar. Rodar com APPDATA apontando para uma pasta de teste (não mexe nas
/// configurações de quem usa o app). `AJ_AUTOTEST=host`: transmite a tela principal (fora
/// da lista "Ao vivo") e grava o código em `AJ_CODE_FILE`. `AJ_AUTOTEST=watch`: assiste o
/// código desse arquivo a `AJ_VIEW_FPS`; com `AJ_MENU_AFTER` (s), abre o menu de engrenagem;
/// com `AJ_SHOW_CONTROLS`, deixa os controles por cima do vídeo à mostra; com
/// `AJ_FULLSCREEN`, põe a stream em tela cheia; com `AJ_WATCH_N`, assiste em vários tiles;
/// com `AJ_FOCUS_TOGGLE`, amplia e reduz a primeira stream a cada 2 s (`fs`: tela cheia;
/// `menu`: abre e fecha o menu de engrenagem); com `AJ_MENU_CLOSE_AFTER` (s), fecha o
/// menu de novo.
#[cfg(debug_assertions)]
fn autotest() {
    let Ok(mode) = std::env::var("AJ_AUTOTEST") else { return };
    let code_file = std::env::var("AJ_CODE_FILE").unwrap_or_else(|_| std::env::temp_dir().join("aj_autotest_code.txt").to_string_lossy().into_owned());
    let timer = Box::leak(Box::new(slint::Timer::default()));
    let started = Instant::now();
    let mut done = false;
    let mut menu_opened = false;
    let mut last_toggle = 0u64;
    let mut quality_set = false;
    let mut fullscreen_done = false;
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(500), move || {
        with(|c, ui| match mode.as_str() {
            "host" => {
                // AJ_HOST_AFTER (s): espera antes de começar (o app já aberto, como na vida real).
                let after = std::env::var("AJ_HOST_AFTER").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                if !done && started.elapsed() < Duration::from_secs(after) {
                    return;
                }
                if !done {
                    done = true;
                    // AJ_PUBLIC: aparece no "Ao vivo agora" (testar a fofoca). Só numa rede de
                    // teste isolada (AJ_TEST_NET): este autoteste transmite a tela de quem roda,
                    // e a lista de verdade é vista por todo mundo.
                    let public = std::env::var_os("AJ_PUBLIC").is_some();
                    if public && rendezvous::test_net().is_none() {
                        eprintln!("[autotest] AJ_PUBLIC ignorado: só com AJ_TEST_NET (rede de teste isolada)");
                    }
                    c.s.public = public && rendezvous::test_net().is_some();
                    c.s.stream_password.clear();
                    c.s.name = "Autoteste".into();
                    // Sem áudio: com várias instâncias na mesma máquina, o host captaria o som
                    // dos espectadores de teste (eco).
                    c.s.audio_mode = "none".into();
                    // AJ_HOST_WINDOW: transmite a janela cujo título contém esse texto (em vez da
                    // tela, que mostraria o próprio espectador em espelho infinito).
                    let want = std::env::var("AJ_HOST_WINDOW").ok();
                    let Some(src) = capture::list_sources().into_iter().find(|s| match &want {
                        Some(w) => s.kind == "window" && s.name.contains(w.as_str()),
                        None => s.primary,
                    }) else { return };
                    start_sharing(c, ui, src, None);
                } else if !c.code.is_empty() && std::fs::read_to_string(&code_file).ok().as_deref() != Some(c.code.as_str()) {
                    let _ = std::fs::write(&code_file, &c.code);
                    eprintln!("[autotest] código gravado");
                }
            }
            "watch" => {
                if !done {
                    let Ok(code) = std::fs::read_to_string(&code_file) else { return };
                    done = true;
                    c.s.view_fps = std::env::var("AJ_VIEW_FPS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
                    eprintln!("[autotest] assistindo a {} fps (monitor {} Hz)", view_fps(c, ui), monitor_hz(ui));
                    // AJ_WATCH_N: a mesma transmissão em vários tiles (grade).
                    let n = std::env::var("AJ_WATCH_N").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 4);
                    for _ in 0..n {
                        start_watching(c, ui, code.trim().to_string(), None);
                    }
                }
                // Amplia e reduz a primeira stream a cada 2 s, pelo mesmo caminho do botão.
                // AJ_TOGGLE_AFTER (s): só começa depois desse tempo.
                let after = std::env::var("AJ_TOGGLE_AFTER").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                if std::env::var_os("AJ_FOCUS_TOGGLE").is_some() && started.elapsed() >= Duration::from_secs(after) && c.viewers.iter().all(|v| v.watching) && !c.viewers.is_empty() {
                    let tick = (started.elapsed().as_millis() / 2000) as u64;
                    if tick != last_toggle {
                        last_toggle = tick;
                        let id = c.viewers[0].id.clone();
                        match std::env::var("AJ_FOCUS_TOGGLE").as_deref() {
                            // Tela cheia (botão de tela cheia), em vez de ampliar.
                            Ok("fs") => {
                                toggle_stream_fullscreen(c, ui, &id);
                                eprintln!("[autotest] {:.0} ms: {}", tms(), if c.fullscreen { "ampliada (tela cheia)" } else { "grade" });
                            }
                            // Abre e fecha o menu de engrenagem.
                            Ok("menu") => {
                                toggle_menu(c, ui, &id);
                                eprintln!("[autotest] {:.0} ms: {}", tms(), if c.menu.is_some() { "ampliada (menu)" } else { "grade" });
                            }
                            _ => {
                                c.focus = if c.focus.is_some() { None } else { Some(id) };
                                eprintln!("[autotest] {:.0} ms: {}", tms(), if c.focus.is_some() { "ampliada" } else { "grade" });
                                relayout(c, ui);
                            }
                        }
                    }
                }
                // Qualidade fixa (AJ_QUALITY=1080, por exemplo), sem a automática.
                if let (Ok(q), false) = (std::env::var("AJ_QUALITY"), quality_set) {
                    if let Some(v) = c.viewers.iter_mut().find(|v| v.watching) {
                        quality_set = true;
                        v.quality = q.clone();
                        session::viewer_quality(&v.id, &q);
                        eprintln!("[autotest] qualidade fixa {q}");
                    }
                }
                // Abre e fecha o menu a cada 2 s, pelo mesmo caminho do clique (medir com AJ_TRACE).
                if std::env::var_os("AJ_MENU_TOGGLE").is_some() && started.elapsed() >= Duration::from_secs(40) {
                    let tick = (started.elapsed().as_millis() / 2000) as u64;
                    if tick != last_toggle {
                        last_toggle = tick;
                        if let Some(id) = c.viewers.iter().find(|v| v.watching).map(|v| v.id.clone()) {
                            eprintln!("[ui {:.0}] (autoteste) alterna o menu", tms());
                            toggle_menu(c, ui, &id);
                        }
                    }
                }
                // Stream em tela cheia (AJ_FULLSCREEN).
                if std::env::var_os("AJ_FULLSCREEN").is_some() && !c.fullscreen && !fullscreen_done {
                    if let Some(id) = c.viewers.iter().find(|v| v.watching).map(|v| v.id.clone()) {
                        fullscreen_done = true;
                        toggle_stream_fullscreen(c, ui, &id);
                        eprintln!("[autotest] tela cheia");
                    }
                }
                // Controles por cima do vídeo sempre à mostra (para conferir sem mexer o mouse).
                if std::env::var_os("AJ_SHOW_CONTROLS").is_some() {
                    if let Some(id) = c.viewers.iter().find(|v| v.watching).map(|v| v.id.clone()) {
                        wake_controls(c, ui, &id);
                    }
                }
                let menu_after = std::env::var("AJ_MENU_AFTER").ok().and_then(|v| v.parse::<u64>().ok());
                if let (Some(after), false) = (menu_after, menu_opened) {
                    if started.elapsed() >= Duration::from_secs(after) {
                        if let Some(v) = c.viewers.iter().find(|v| v.watching) {
                            menu_opened = true;
                            c.menu = Some(v.id.clone());
                            c.menu_panel = std::env::var("AJ_MENU_PANEL").unwrap_or_else(|_| "main".into());
                            relayout(c, ui);
                            eprintln!("[autotest] menu aberto");
                        }
                    }
                }
                // Fecha o menu de novo (como o segundo clique na engrenagem).
                let close_after = std::env::var("AJ_MENU_CLOSE_AFTER").ok().and_then(|v| v.parse::<u64>().ok());
                if let (Some(after), true) = (close_after, menu_opened && c.menu.is_some()) {
                    if started.elapsed() >= Duration::from_secs(after) {
                        let id = c.menu.clone().unwrap_or_default();
                        toggle_menu(c, ui, &id);
                        eprintln!("[autotest] menu fechado");
                    }
                }
            }
            _ => {}
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn sync_updates_in_place() {
        let m = model(vec![1, 2, 3]);
        let replaced = Rc::new(Cell::new(false));
        let r = replaced.clone();
        sync(m.clone(), vec![1, 5, 3, 4], move |_| r.set(true));
        assert!(!replaced.get(), "não deve trocar o modelo inteiro");
        assert_eq!(m.iter().collect::<Vec<_>>(), vec![1, 5, 3, 4]);
        sync(m.clone(), vec![1, 5], |_| panic!("trocou tudo"));
        assert_eq!(m.iter().collect::<Vec<_>>(), vec![1, 5]);
        // Modelo que não é VecModel (o vazio inicial do Slint): troca.
        let replaced = Rc::new(Cell::new(false));
        let r = replaced.clone();
        sync(ModelRc::<i32>::default(), vec![7], move |_| r.set(true));
        assert!(replaced.get());
    }

    #[test]
    fn stream_fps_in_the_menu_is_stable() {
        assert_eq!(snap_fps(57.4), 60);
        assert_eq!(snap_fps(61.0), 60);
        assert_eq!(snap_fps(29.2), 30);
        assert_eq!(snap_fps(118.0), 120);
        assert_eq!(snap_fps(40.0), 40);
        assert_eq!(snap_fps(0.0), 0);
        let mut v = Viewer {
            id: String::new(), code: String::new(), host_name: String::new(), step: 0, busy: false, watching: true,
            stats: None, ping: None, volume: 100, muted: false, quality: "auto".into(), renditions: vec![],
            nominal_fps: 0, fps_candidate: (0, 0),
        };
        for m in [58.0, 61.0, 57.0, 60.0, 62.0] {
            v.update_nominal_fps(m);
            assert_eq!(v.nominal_fps, 60);
        }
        // Uma medição fora não muda; duas seguidas mudam (o host baixou para 30).
        v.update_nominal_fps(31.0);
        assert_eq!(v.nominal_fps, 60);
        v.update_nominal_fps(30.0);
        assert_eq!(v.nominal_fps, 30);
    }

    #[test]
    fn sync_rows_keeps_each_row_model() {
        let rows = model(vec![StreamRow { items: model(vec![Stream { name: s("a"), ..Default::default() }]) }]);
        let inner_before = rows.row_data(0).unwrap().items;
        sync_rows(rows.clone(), vec![vec![Stream { name: s("b"), ..Default::default() }]], |r| r.items.clone(), |items| StreamRow { items }, |_| panic!("trocou tudo"));
        let inner_after = rows.row_data(0).unwrap().items;
        assert!(inner_before == inner_after, "a linha devia manter o mesmo modelo de itens");
        assert_eq!(inner_after.row_data(0).unwrap().name, "b");
    }
}
