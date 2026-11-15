import { create } from "zustand";

export type View = "home" | "host" | "watch";
export type Theme = "light" | "dark";
export type AudioMode = "none" | "system" | "exclude" | "include";
export type PeerState = "connecting" | "connected" | "unstable";
export type Step = "prepare" | "publish" | "wait" | "connect";

export interface StreamSettings {
  res: number;
  fps: number;
  bitrate: number;
  cursor: boolean;
  audioMode: AudioMode | null;
  audioApp: string;
  abitrate: number;
  volume: number;
  /** Aparecer na lista pública de transmissões. */
  public: boolean;
}

export interface Peer { id: number; name: string; state: PeerState }
export interface Stat { label: string; value: string; warn?: boolean }
export interface AudioApp { name: string; exe: string; active: boolean }
export interface Source {
  id: string;
  kind: "monitor" | "window";
  name: string;
  app: string;
  width: number;
  height: number;
  primary: boolean;
  thumbnail: string;
}
export interface HostInfo {
  encoder: string; hardware: boolean; zero_copy: boolean; width: number; height: number;
  fps: number; kbps: number; target_kbps: number; encode_ms: number; captured_fps: number;
}
export interface PublicStream {
  id: string;
  name: string;
  title: string;
  code: string;
  viewers: number;
  quality: string;
  since: number;
  ts: number;
  own: boolean;
}
export interface ViewStats { width: number; height: number; fps: number; kbps: number; buffer_ms: number }
/** Uma transmissão que estou assistindo (um tile na grade). */
export interface Viewer {
  id: string;
  code: string;
  hostName: string;
  step: Step | null;
  watching: boolean;
  stats: ViewStats | null;
  ping: number | null;
  volume: number;
  muted: boolean;
}

const DEFAULTS: StreamSettings = {
  res: 1080, fps: 60, bitrate: 0, cursor: true,
  audioMode: null, audioApp: "", abitrate: 192, volume: 100, public: true,
};

function load<T>(key: string, fallback: T): T {
  try {
    const v = localStorage.getItem("aj." + key);
    return v === null ? fallback : (JSON.parse(v) as T);
  } catch { return fallback; }
}
export function save(key: string, value: unknown) {
  try { localStorage.setItem("aj." + key, JSON.stringify(value)); } catch { /* sem storage */ }
}

function initialTheme(): Theme {
  const saved = load<string>("theme", "");
  if (saved === "light" || saved === "dark") return saved;
  try { return matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"; } catch { return "dark"; }
}

interface State {
  view: View;
  theme: Theme;
  net: "pending" | "ok" | "err";
  name: string;
  settings: StreamSettings;
  apps: AudioApp[];

  // Lista pública (dashboard do Início)
  publicStreams: PublicStream[] | null;
  publicLoading: boolean;
  publicUpdated: number;

  // Seletor de fontes
  picker: null | "start" | "switch";
  sources: Source[] | null;

  // Transmitindo
  live: boolean;
  source: Source | null;
  code: string;
  preview: string;
  info: HostInfo | null;
  peers: Peer[];
  audioStatus: { text: string; kind: "" | "ok" | "err" };
  audioLevel: number;

  // Assistindo (várias ao mesmo tempo)
  viewers: Viewer[];
  /** Stream ampliada (null = grade). */
  focus: string | null;
  defaultVolume: number;
  fullscreen: boolean;
}

export const useApp = create<State>(() => ({
  view: "home",
  theme: initialTheme(),
  net: "pending",
  name: load("name", ""),
  settings: { ...DEFAULTS, ...load<Partial<StreamSettings>>("stream", {}) },
  apps: [],
  publicStreams: null,
  publicLoading: false,
  publicUpdated: 0,
  picker: null,
  sources: null,
  live: false,
  source: null,
  code: "",
  preview: "",
  info: null,
  peers: [],
  audioStatus: { text: "Sem áudio", kind: "" },
  audioLevel: 0,
  viewers: [],
  focus: null,
  defaultVolume: load("viewVolume", 100),
  fullscreen: false,
}));

export const getState = useApp.getState;
export const setState = useApp.setState;

/** Troca entre claro e escuro e lembra a escolha. */
export function setTheme(theme: Theme) {
  document.documentElement.classList.toggle("dark", theme === "dark");
  setState({ theme });
  save("theme", theme);
}
