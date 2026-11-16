//! Controlador da interface: liga a janela Slint ao motor.
//!
//! Faz o papel que antes era do `engine.ts` + `store.ts`: guarda o estado da interface,
//! recebe os eventos do motor (sempre na thread da interface), chama o motor em threads
//! próprias e posiciona os players nativos sobre os tiles da tela "Assistindo".

use crate::app::AppHandle;
use crate::{audio, capture, rendezvous, session, settings, transport, updater};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

slint::include_modules!();

// Medidas fixas da tela "Assistindo" (iguais às do watch.slint).
const TOPBAR: f32 = 44.0;
const ADDBAR: f32 = 60.0;
const PAD: f32 = 12.0;
const GAP: f32 = 12.0;
const HEADER: f32 = 44.0;
const FOOTER: f32 = 40.0;

const PUBLIC_REFRESH: Duration = Duration::from_secs(20);
const TOAST_TIME: Duration = Duration::from_secs(4);
const CONTROLS_TIME: Duration = Duration::from_millis(2500);

struct Viewer {
    id: String,
    code: String,
    host_name: String,
    step: i32,
    watching: bool,
    stats: Option<Value>,
    ping: Option<u32>,
    volume: u32,
    muted: bool,
}

struct Ctl {
    ui: slint::Weak<AppWindow>,
    app: AppHandle,
    s: settings::Settings,
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
    sources: Option<Vec<capture::Source>>,
    thumbs: HashMap<String, Image>,
    picker: String,
    picker_tab: String,
    picker_selected: String,
    viewers: Vec<Viewer>,
    focus: Option<String>,
    fullscreen: bool,
    controls_until: Option<Instant>,
    toasts: Vec<(i32, String, String, Instant)>,
    toast_seq: i32,
    updating: bool,
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

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
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
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = ui.window().window_handle();
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
    a.set_view(s(&c.view));
    a.set_net(s(&c.net));
    a.set_live(c.live);
    a.set_host_viewers(c.host_viewers as i32);
    a.set_watch_count(c.viewers.len() as i32);
    a.set_fullscreen(c.fullscreen);
    a.set_title(s(if c.host_viewers > 0 { format!("AntiJanja ({} assistindo)", c.host_viewers) } else { "AntiJanja".into() }));
}

fn render_settings(c: &Ctl, ui: &AppWindow) {
    let a = ui.global::<App>();
    let st = &c.s;
    a.set_s_res(s(st.res.to_string()));
    a.set_s_fps(s(st.fps.to_string()));
    a.set_s_bitrate(s(st.bitrate.to_string()));
    a.set_s_cursor(st.cursor);
    a.set_s_audio_mode(s(if st.audio_mode.is_empty() { "system" } else { &st.audio_mode }));
    a.set_s_audio_app(s(&st.audio_app));
    a.set_s_abitrate(s(st.abitrate.to_string()));
    a.set_s_volume(st.volume as i32);
    a.set_s_public(st.public);

    let mut options: Vec<SelectOption> = c
        .apps
        .iter()
        .map(|app| SelectOption { value: s(&app.exe), label: s(if app.active { format!("{} · tocando agora", app.name) } else { app.name.clone() }) })
        .collect();
    if !st.audio_app.is_empty() && !c.apps.iter().any(|app| app.exe == st.audio_app) {
        options.push(SelectOption { value: s(&st.audio_app), label: s(format!("{} (fechado)", app_name(c, &st.audio_app))) });
    }
    a.set_app_options(model(options));
    a.set_apps(model(c.apps.iter().map(|x| AudioApp { exe: s(&x.exe), name: s(&x.name), active: x.active }).collect()));
}

fn render_public(c: &Ctl, ui: &AppWindow) {
    let a = ui.global::<App>();
    a.set_public_busy(c.public_busy);
    a.set_public_loading(c.public.is_none());
    a.set_public_updated(s(if c.public_busy {
        "Atualizando".to_string()
    } else {
        match c.public_updated.map(|t| t.elapsed().as_secs()) {
            None => "Atualizar".into(),
            Some(n) if n < 5 => "agora".into(),
            Some(n) => format!("há {n} s"),
        }
    }));
    let list = c.public.as_deref().unwrap_or(&[]);
    let others: Vec<&rendezvous::PublicStream> = list.iter().filter(|p| !p.own).collect();
    a.set_public_mine(list.iter().any(|p| p.own));
    a.set_public_count(others.len() as i32);
    a.set_public_watching(others.iter().map(|p| p.entry.viewers as i32).sum());
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
        }
    };
    a.set_has_featured(!others.is_empty());
    if let Some(first) = others.first() {
        a.set_featured(to_stream(first));
    }
    let rows: Vec<StreamRow> = others
        .iter()
        .skip(1)
        .copied()
        .collect::<Vec<_>>()
        .chunks(3)
        .map(|chunk| {
            let mut items: Vec<Stream> = chunk.iter().map(|p| to_stream(p)).collect();
            items.resize_with(3, || Stream { blank: true, ..Default::default() });
            StreamRow { items: model(items) }
        })
        .collect();
    a.set_rows(model(rows));
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
                STEP_TEXT[v.step.clamp(0, 3) as usize].to_string()
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
    ui.global::<App>().set_watching_list(model(items));
}

fn render_picker(c: &mut Ctl, ui: &AppWindow) {
    let a = ui.global::<App>();
    a.set_picker(s(&c.picker));
    a.set_picker_tab(s(&c.picker_tab));
    a.set_picker_selected(s(&c.picker_selected));
    a.set_sources_loading(c.sources.is_none());
    let list = c.sources.as_deref().unwrap_or(&[]);
    a.set_count_monitors(list.iter().filter(|x| x.kind == "monitor").count() as i32);
    a.set_count_windows(list.iter().filter(|x| x.kind == "window").count() as i32);
    let shown: Vec<&capture::Source> = list.iter().filter(|x| x.kind == c.picker_tab).collect();
    let cols = if c.picker_tab == "monitor" && shown.len() <= 2 { 2 } else { 3 };
    let current = if c.picker == "switch" { c.source.as_ref().map(|x| x.id.clone()) } else { None };
    let rows: Vec<SourceRow> = shown
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
            SourceRow { items: model(items) }
        })
        .collect();
    a.set_source_rows(model(rows));
}

fn render_host_code(c: &Ctl, ui: &AppWindow) {
    let a = ui.global::<App>();
    a.set_code(s(&c.code));
    let (prefix, body) = if c.code.len() > 4 { c.code.split_at(4) } else { (c.code.as_str(), "") };
    a.set_code_prefix(s(prefix));
    let groups: Vec<String> = body.chars().collect::<Vec<_>>().chunks(4).map(|g| g.iter().collect()).collect();
    a.set_code_body(s(groups.join(" ")));
    a.set_source_name(s(c.source.as_ref().map(|x| x.name.as_str()).unwrap_or("")));
}

fn render_toasts(c: &Ctl, ui: &AppWindow) {
    let items: Vec<Toast> = c.toasts.iter().map(|(id, text, kind, _)| Toast { id: *id, text: s(text), kind: s(kind) }).collect();
    ui.global::<App>().set_toasts(model(items));
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
    let a = ui.global::<App>();
    let scale = ui.window().scale_factor();
    let size = ui.window().size();
    let (w, h) = (size.width as f32 / scale, size.height as f32 / scale);
    let fs = c.fullscreen;
    let top = if fs { 0.0 } else { TOPBAR + ADDBAR };
    let pad = if fs { 0.0 } else { PAD };
    let gap = if fs { 0.0 } else { GAP };
    let area_w = (w - 2.0 * pad).max(1.0);
    let area_h = (h - top - pad).max(1.0);
    let controls = !fs || c.controls_until.is_some_and(|t| t > Instant::now());

    let shown: Vec<&Viewer> = match &c.focus {
        Some(f) => c.viewers.iter().filter(|v| &v.id == f).collect(),
        None => c.viewers.iter().collect(),
    };
    let (cols, rows) = grid_for(shown.len());
    let tile_w = (area_w - gap * (cols as f32 - 1.0)) / cols as f32;
    let tile_h = (area_h - gap * (rows as f32 - 1.0)) / rows as f32;
    let players_visible = c.view == "watch" && c.picker.is_empty() && !c.updating;

    let mut tiles = Vec::new();
    for (i, v) in shown.iter().enumerate() {
        let (col, row) = ((i % cols) as f32, (i / cols) as f32);
        let x = pad + col * (tile_w + gap);
        let y = row * (tile_h + gap);
        let border = if fs { 0.0 } else { 1.0 };
        let head = if controls { HEADER } else { 0.0 };
        let foot = if controls && v.watching { FOOTER } else { 0.0 };
        let (vx, vy) = (x + border, top + y + head);
        let (vw, vh) = (tile_w - 2.0 * border, (tile_h - head - foot - border).max(1.0));
        session::player_rect(
            v.id.clone(),
            (vx * scale).round() as i32,
            (vy * scale).round() as i32,
            (vw * scale).round() as i32,
            (vh * scale).round() as i32,
            players_visible && v.watching,
        );

        let stats = v.stats.as_ref();
        let num = |k: &str| stats.and_then(|st| st[k].as_f64()).unwrap_or(0.0);
        let buffer = num("buffer_ms") as u32;
        tiles.push(Tile {
            id: s(&v.id),
            name: s(if v.host_name.is_empty() { "Conectando…" } else { &v.host_name }),
            initials: s(initials(if v.host_name.is_empty() { "?" } else { &v.host_name })),
            watching: v.watching,
            step: v.step,
            res: s(if stats.is_some() { format!("{}p {}fps", num("height") as u32, num("fps").round() as u32) } else { String::new() }),
            rate: s(format_rate(num("kbps"))),
            ping: s(v.ping.map(|p| if buffer > 0 { format!("{p} ms +{buffer}") } else { format!("{p} ms") }).unwrap_or_default()),
            ping_warn: v.ping.is_some_and(|p| p > 150),
            volume: v.volume as i32,
            muted: v.muted,
            chrome: controls,
            focused: c.focus.as_deref() == Some(v.id.as_str()),
            x,
            y,
            w: tile_w,
            h: tile_h,
        });
    }
    // Streams escondidas (outra está ampliada): player fora da tela.
    for v in c.viewers.iter().filter(|v| !shown.iter().any(|x| x.id == v.id)) {
        session::player_rect(v.id.clone(), 0, 0, 1, 1, false);
    }
    a.set_tiles(model(tiles));
    a.set_has_focus(c.focus.is_some());
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
    c.controls_until = on.then(|| Instant::now() + CONTROLS_TIME);
    ui.window().set_fullscreen(on);
    render_general(c, ui);
    relayout(c, ui);
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
            Ok(list) => {
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
    })
}

fn my_name(c: &Ctl) -> String {
    let n = c.s.name.trim();
    if n.is_empty() { "Anônimo".into() } else { n.chars().take(40).collect() }
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
    settings::save(&c.s);
    render_settings(c, ui);
    push_config(c, ui);
}

fn open_picker(c: &mut Ctl, ui: &AppWindow, mode: &str) {
    // O seletor abre fora da grade: lá os vídeos nativos o cobririam.
    if c.view == "watch" {
        c.view = if c.live { "host".into() } else { "home".into() };
    }
    c.picker = mode.to_string();
    c.picker_selected.clear();
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
    a.set_has_preview(preview.is_some());
    if let Some(p) = preview {
        a.set_preview(p);
    }
    a.set_has_info(false);
    a.set_peers(model(Vec::new()));
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
    if let Err(e) = rendezvous::check_code(&code) {
        return toast(c, ui, "err", e);
    }
    if c.live && c.code == code {
        return toast(c, ui, "err", "Esse é o código da sua própria transmissão.");
    }
    if c.viewers.iter().any(|v| v.code == code) {
        set_view(c, ui, "watch");
        return toast(c, ui, "", "Você já está assistindo essa transmissão.");
    }
    let id = random_id();
    let volume = c.s.view_volume.min(100);
    c.viewers.push(Viewer { id: id.clone(), code: code.clone(), host_name: String::new(), step: 0, watching: false, stats: None, ping: None, volume, muted: false });
    c.focus = None;
    c.view = "watch".into();
    render_general(c, ui);
    render_watching_list(c, ui);
    render_public(c, ui);
    relayout(c, ui);

    let (app, name) = (c.app.clone(), my_name(c));
    transport::tokio_rt().spawn(async move {
        let r = session::viewer_join(app, id.clone(), code, name, volume).await;
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

fn end_watching(c: &mut Ctl, ui: &AppWindow, id: &str, message: Option<&str>, is_error: bool) {
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
    settings::save(&c.s);
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
            a.set_peers(model(peers));
            render_general(c, ui);
        }
        "host-info" => {
            let f = |k: &str| v[k].as_f64().unwrap_or(0.0);
            let hw = v["hardware"].as_bool().unwrap_or(false);
            a.set_has_info(true);
            a.set_hardware(hw);
            a.set_stat_res(s(format!("{}×{}", f("width") as u32, f("height") as u32)));
            a.set_stat_fps(s((f("fps").round() as u32).to_string()));
            a.set_stat_rate(s(format!("{} de {}", format_rate(f("kbps")), format_rate(f("target_kbps")))));
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
                a.set_preview(Image::from_rgba8(buf));
                a.set_has_preview(true);
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
                    a.set_audio_level(0.0);
                    ("Sem áudio".to_string(), "")
                }
            };
            a.set_audio_status(s(text));
            a.set_audio_status_kind(s(kind));
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
            let step = match v["value"].as_str().unwrap_or("") {
                "publish" => 1,
                "wait" => 2,
                "connect" => 3,
                _ => 0,
            };
            if let Some(x) = c.viewers.iter_mut().find(|x| x.id == id && !x.watching) {
                x.step = step;
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
            if let Some(x) = c.viewers.iter_mut().find(|x| x.id == id) {
                x.stats = Some(v);
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
                "dblclick" => {
                    let focused = c.focus.as_deref() == Some(id.as_str());
                    c.focus = if focused { None } else { Some(id) };
                    set_fullscreen(c, ui, !focused);
                    relayout(c, ui);
                }
                "move" if c.fullscreen => {
                    let was_hidden = !c.controls_until.is_some_and(|t| t > Instant::now());
                    c.controls_until = Some(Instant::now() + CONTROLS_TIME);
                    if was_hidden {
                        relayout(c, ui);
                    }
                }
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
            let _ = c;
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
            if !c.picker.is_empty() {
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
    a.on_unfocus(|| {
        later(|c, ui| {
            c.focus = None;
            relayout(c, ui);
        })
    });
    a.on_set_volume(|id, vol| later(move |c, ui| set_viewer_volume(c, ui, &id, vol.clamp(0, 100) as u32, false)));
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
            sources: None,
            thumbs: HashMap::new(),
            picker: String::new(),
            picker_tab: "monitor".into(),
            picker_selected: String::new(),
            viewers: Vec::new(),
            focus: None,
            fullscreen: false,
            controls_until: None,
            toasts: Vec::new(),
            toast_seq: 0,
            updating: false,
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

    // Ao fechar: encerra a transmissão e as conexões antes de sair.
    ui.window().on_close_requested(|| {
        with(|c, _| {
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

    // A janela nativa só existe depois que o loop começa: pega o HWND e pinta a barra.
    let hwnd_timer = slint::Timer::default();
    {
        let app = app.clone();
        let weak = ui.as_weak();
        hwnd_timer.start(slint::TimerMode::Repeated, Duration::from_millis(30), move || {
            let Some(ui) = weak.upgrade() else { return };
            if app.hwnd() != 0 {
                return;
            }
            if let Some(h) = hwnd_of(&ui) {
                app.set_hwnd(h);
                paint_titlebar(h, ui.global::<Theme>().get_dark());
                with(|c, ui| relayout(c, ui));
            }
        });
    }

    // Lista pública a cada 20 s (no Início); relógio dos textos "há X s"; avisos que expiram;
    // controles que somem em tela cheia.
    let public_timer = slint::Timer::default();
    public_timer.start(slint::TimerMode::Repeated, PUBLIC_REFRESH, || {
        with(|c, ui| {
            if c.view == "home" {
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
            if c.fullscreen && c.controls_until.is_some_and(|t| t <= now) {
                c.controls_until = None;
                relayout(c, ui);
            }
        });
    });
    let clock = slint::Timer::default();
    clock.start(slint::TimerMode::Repeated, Duration::from_secs(5), || {
        with(|c, ui| render_public(c, ui));
    });

    with(|c, ui| refresh_apps(c, ui));
    start_updater();

    slint::run_event_loop_until_quit()
}
