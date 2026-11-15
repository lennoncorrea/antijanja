/*
 * Ponte entre a interface e o motor nativo (Rust).
 * O vídeo e o áudio não passam por aqui: captura, codificação, transporte, decodificação
 * e exibição são todos nativos. Esta camada só manda comandos e ouve eventos.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { toast } from "sonner";
import { errMsg } from "@/lib/rtc";
import {
  getState, save, setState,
  type AudioApp, type HostInfo, type PublicStream, type Peer, type Source, type Step, type StreamSettings, type Viewer, type ViewStats,
} from "@/store";

export const appWindow = getCurrentWindow();
const myName = () => getState().name.trim() || "Anônimo";

/* =============================================================== FONTES */

export async function openPicker(mode: "start" | "switch") {
  // O seletor abre na tela de transmissão/início: na grade, os vídeos nativos o cobririam.
  const view = getState().view === "watch" ? (getState().live ? "host" : "home") : getState().view;
  setState({ picker: mode, sources: null, view });
  refreshApps();
  await loadSources();
}

export async function loadSources() {
  setState({ sources: null });
  try {
    const sources = await invoke<Source[]>("sources_list");
    setState({ sources });
  } catch (e) {
    toast.error("Não deu para listar as telas: " + errMsg(e));
    setState({ sources: [] });
  }
}

export const closePicker = () => setState({ picker: null });

/* ========================================================= LISTA PÚBLICA */

let publicBusy = false;
/** Lê o mural público da DHT (quem está transmitindo agora). */
export async function refreshPublic() {
  if (publicBusy) return;
  publicBusy = true;
  setState({ publicLoading: true });
  try {
    const list = await invoke<PublicStream[]>("public_list");
    setState({ publicStreams: list, publicUpdated: Date.now() });
  } catch {
    if (getState().publicStreams === null) setState({ publicStreams: [] });
  } finally {
    publicBusy = false;
    setState({ publicLoading: false });
  }
}

/* ================================================================= HOST */

function hostConfig(source: Source) {
  const s = getState().settings;
  return {
    source: source.id,
    name: myName(),
    res: s.res,
    fps: s.fps,
    bitrate: s.bitrate,
    cursor: s.cursor,
    audioMode: s.audioMode ?? "none",
    audioApp: s.audioApp,
    abitrate: s.abitrate,
    volume: s.volume,
    public: s.public,
    title: source.name,
  };
}

export async function startSharing(source: Source) {
  closePicker();
  setState({ view: "host", live: true, source, code: "", preview: source.thumbnail, info: null, peers: [] });
  try {
    const code = await invoke<string>("host_start", { config: hostConfig(source) });
    setState({ code });
  } catch (e) {
    toast.error(errMsg(e));
    setState({ view: "home", live: false });
  }
}

/** Troca a tela/janela transmitida sem mudar o código. */
export async function switchSource(source: Source) {
  closePicker();
  setState({ source, preview: source.thumbnail });
  await pushConfig();
  toast.success(`Agora transmitindo: ${source.name}`);
}

async function pushConfig() {
  const { live, source } = getState();
  if (!live || !source) return;
  try { await invoke("host_update", { config: hostConfig(source) }); }
  catch (e) { toast.error(errMsg(e)); }
}

export function updateSettings(patch: Partial<StreamSettings>) {
  const settings = { ...getState().settings, ...patch };
  setState({ settings });
  save("stream", settings);
  pushConfig();
}

export async function newCode() {
  try {
    const code = await invoke<string>("host_new_code");
    setState({ code });
    toast.success("Código novo gerado. O anterior parou de funcionar.");
  } catch (e) { toast.error(errMsg(e)); }
}

export const kickPeer = (id: number) => invoke("host_kick", { id });

export async function stopSharing(reason?: string) {
  if (!getState().live) return;
  await invoke("host_stop").catch(() => {});
  setState({ live: false, view: "home", code: "", preview: "", info: null, peers: [] });
  document.title = "AntiJanja";
  if (reason) toast(reason);
}

export async function refreshApps() {
  let apps: AudioApp[] = [];
  try { apps = await invoke<AudioApp[]>("audio_apps"); } catch { /* sem lista */ }
  setState({ apps });
  const s = getState().settings;
  const discord = apps.find((a) => a.exe.startsWith("discord"));
  // Primeira vez: se o Discord estiver aberto, já tira ele da transmissão.
  if (!s.audioMode) updateSettings({ audioMode: discord ? "exclude" : "system", audioApp: discord?.exe || "" });
  else if (!s.audioApp && discord) updateSettings({ audioApp: discord.exe });
}

export const appName = (exe?: string) =>
  getState().apps.find((a) => a.exe === exe)?.name || exe?.replace(/\.exe$/i, "") || "";

export async function copyCode(code: string) {
  try { await navigator.clipboard.writeText(code); toast.success("Código copiado. Agora é só enviar."); }
  catch { toast.error("Não deu para copiar. Selecione o código e use Ctrl+C."); }
}

/* ============================================================ ESPECTADOR */
/* Várias transmissões ao mesmo tempo: cada uma é um tile com ID próprio. */

const viewerById = (id: string) => getState().viewers.find((v) => v.id === id);

function patchViewer(id: string, patch: Partial<Viewer>) {
  setState({ viewers: getState().viewers.map((v) => (v.id === id ? { ...v, ...patch } : v)) });
}

export async function joinWithCode(raw: string) {
  const code = raw.trim();
  if (!code) return toast.error("Cole o código da transmissão primeiro.");
  if (!code.startsWith("AJ2.")) return toast.error("Esse não é um código do AntiJanja. Ele começa com AJ2.");
  if (getState().live && getState().code === code) return toast.error("Esse é o código da sua própria transmissão.");
  const existing = getState().viewers.find((v) => v.code === code);
  if (existing) {
    setState({ view: "watch" });
    return toast("Você já está assistindo essa transmissão.");
  }
  const id = crypto.randomUUID();
  const volume = getState().defaultVolume;
  const viewer: Viewer = { id, code, hostName: "", step: "prepare", watching: false, stats: null, ping: null, volume, muted: false };
  setState({ viewers: [...getState().viewers, viewer], view: "watch", focus: null });
  try {
    const hostName = await invoke<string>("viewer_join", { id, code, name: myName(), volume });
    if (viewerById(id)) patchViewer(id, { hostName });
  } catch (e) {
    if (errMsg(e) === "cancelado") return;
    endWatching(id, errMsg(e), true);
  }
}

export async function endWatching(id: string, message?: string, isError = false) {
  const v = viewerById(id);
  if (!v) return;
  const { focus } = getState();
  setState({ viewers: getState().viewers.filter((x) => x.id !== id), focus: focus === id ? null : focus });
  if (focus === id) await setFullscreen(false);
  invoke("viewer_leave", { id }).catch(() => {});
  if (message) (isError ? toast.error : toast)(v.hostName ? `${v.hostName}: ${message}` : message);
}

export async function endAllWatching() {
  for (const v of [...getState().viewers]) await endWatching(v.id);
}

export function setViewerVolume(id: string, volume: number, muted: boolean) {
  patchViewer(id, { volume, muted });
  setState({ defaultVolume: volume });
  save("viewVolume", volume);
  invoke("viewer_volume", { id, volume, muted }).catch(() => {});
}

export function setFocus(id: string | null) {
  setState({ focus: id });
  if (!id) setFullscreen(false);
}

/** Posiciona o player nativo de uma stream sobre a área de vídeo (pixels físicos). */
export function setPlayerRect(id: string, rect: DOMRect | null, visible: boolean) {
  const dpr = window.devicePixelRatio || 1;
  const r = rect ?? new DOMRect(0, 0, 1, 1);
  invoke("player_rect", {
    id,
    x: Math.round(r.x * dpr), y: Math.round(r.y * dpr),
    width: Math.round(r.width * dpr), height: Math.round(r.height * dpr),
    visible,
  }).catch(() => {});
}

/* ======================================================== JANELA / INIT */

export async function setFullscreen(on: boolean) {
  if (on === getState().fullscreen) return;
  setState({ fullscreen: on });
  try { await appWindow.setFullscreen(on); } catch { /* ignora */ }
}

/** Eventos de mouse/teclado vindos do vídeo nativo (para mostrar os controles). */
type InputListener = (id: string, kind: string) => void;
const inputListeners = new Set<InputListener>();
export function onPlayerInput(fn: InputListener) {
  inputListeners.add(fn);
  return () => { inputListeners.delete(fn); };
}

export async function closeApp() {
  await stopSharing();
  await endAllWatching();
  appWindow.close();
}

let started = false;
export async function initEngine() {
  if (started) return;
  started = true;

  listen<boolean>("dht-status", ({ payload }) => setState({ net: payload ? "ok" : "err" }));
  invoke<boolean>("rdv_ready").then((ready) => ready && setState({ net: "ok" })).catch(() => {});

  // Host
  listen<Peer[]>("host-peers", ({ payload }) => {
    setState({ peers: payload });
    const n = payload.filter((p) => p.state !== "connecting").length;
    document.title = n ? `AntiJanja (${n} assistindo)` : "AntiJanja";
  });
  listen<HostInfo>("host-info", ({ payload }) => setState({ info: payload }));
  listen<string>("host-preview", ({ payload }) => setState({ preview: payload }));
  listen<number>("host-audio-level", ({ payload }) => setState({ audioLevel: payload }));
  listen<{ status: string; detail: string }>("host-audio", ({ payload: { status, detail } }) => {
    const s = getState().settings;
    if (status === "capturing") {
      const text = s.audioMode === "system" ? "Capturando todo o áudio do PC"
        : s.audioMode === "exclude" ? `Capturando tudo, menos ${appName(s.audioApp)}`
        : `Capturando só ${appName(s.audioApp)}`;
      setState({ audioStatus: { text: detail ? `${text} (${detail})` : text, kind: "ok" } });
    } else if (status === "waiting") {
      setState({ audioStatus: { text: `Esperando ${detail} abrir`, kind: "" } });
    } else if (status === "error") {
      setState({ audioStatus: { text: "Erro no áudio: " + detail, kind: "err" } });
    } else {
      setState({ audioStatus: { text: "Sem áudio", kind: "" }, audioLevel: 0 });
    }
  });
  listen<string>("host-error", ({ payload }) => toast.error(payload));
  listen("host-source-closed", () => {
    toast("A janela transmitida foi fechada. Escolha outra fonte.");
    openPicker("switch");
  });
  listen<string>("host-viewer-joined", ({ payload }) => toast.success(`${payload} está assistindo.`));
  listen<string>("host-viewer-left", ({ payload }) => toast(`${payload} saiu da transmissão.`));

  // Espectador (cada evento traz o ID da stream)
  type Ev<T> = { id: string; value: T };
  listen<Ev<Step>>("viewer-step", ({ payload: { id, value } }) => {
    const v = viewerById(id);
    if (v && !v.watching) patchViewer(id, { step: value });
  });
  listen<Ev<string>>("viewer-state", ({ payload: { id, value } }) => {
    const v = viewerById(id);
    if (!v) return;
    if (value === "connected") patchViewer(id, { watching: true, step: null });
    else if (value === "failed") endWatching(id, v.watching ? "A conexão caiu." : "Não deu para conectar direto. Alguns roteadores bloqueiam conexões P2P.", true);
    else if (value === "closed") endWatching(id, "A transmissão terminou.");
  });
  listen<Ev<string>>("viewer-ended", ({ payload: { id, value } }) => endWatching(id, value));
  listen<ViewStats & { id: string }>("viewer-stats", ({ payload: { id, ...stats } }) => viewerById(id) && patchViewer(id, { stats }));
  listen<Ev<number>>("viewer-ping", ({ payload: { id, value } }) => viewerById(id) && patchViewer(id, { ping: value }));
  listen<{ id: string; kind: string }>("player-input", ({ payload: { id, kind } }) => {
    if (kind === "dblclick") {
      const focused = getState().focus === id;
      setFocus(focused ? null : id);
      setFullscreen(!focused);
    } else if (kind === "f11") {
      setFullscreen(!getState().fullscreen);
    } else if (kind === "escape") {
      setFullscreen(false);
    }
    inputListeners.forEach((fn) => fn(id, kind));
  });

  if (!getState().name) {
    const name = await invoke<string>("device_name").catch(() => "Meu PC");
    setState({ name });
    save("name", name);
  }
  refreshApps();
}
