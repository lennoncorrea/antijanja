import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { Check, ClipboardPaste, LayoutGrid, Loader2, Maximize2, Minimize2, Plus, Tv, Volume1, Volume2, VolumeX, X } from "lucide-react";
import { toast } from "sonner";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Slider } from "@/components/ui/slider";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { endWatching, joinWithCode, onPlayerInput, setFocus, setFullscreen, setPlayerRect, setViewerVolume } from "@/engine";
import { formatRate, initials } from "@/lib/rtc";
import { useApp, type Step, type Viewer } from "@/store";
import { cn } from "@/lib/utils";

const STEPS: { id: Step; label: string }[] = [
  { id: "prepare", label: "Preparando a conexão" },
  { id: "publish", label: "Mandando o pedido pela rede P2P" },
  { id: "wait", label: "Esperando o host responder" },
  { id: "connect", label: "Conectando direto ao host" },
];

function gridFor(n: number) {
  if (n <= 1) return { cols: 1, rows: 1 };
  if (n === 2) return { cols: 2, rows: 1 };
  if (n <= 4) return { cols: 2, rows: 2 };
  if (n <= 6) return { cols: 3, rows: 2 };
  return { cols: 3, rows: Math.ceil(n / 3) };
}

export function WatchView() {
  const viewers = useApp((s) => s.viewers);
  const focus = useApp((s) => s.focus);
  const fullscreen = useApp((s) => s.fullscreen);
  const shown = focus ? viewers.filter((v) => v.id === focus) : viewers;
  const { cols, rows } = gridFor(shown.length);
  const layoutKey = `${shown.map((v) => v.id).join(",")}|${fullscreen}`;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "F11") { e.preventDefault(); setFullscreen(!useApp.getState().fullscreen); }
      if (e.key === "Escape" && useApp.getState().fullscreen) setFullscreen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  return (
    <div className="flex h-full flex-col">
      {!fullscreen && <AddBar count={viewers.length} />}
      {viewers.length === 0 ? (
        <div className="grid flex-1 place-items-center p-6 text-center">
          <div className="grid max-w-sm justify-items-center gap-3">
            <div className="grid size-12 place-items-center rounded-xl bg-muted"><Tv className="size-6 text-muted-foreground" /></div>
            <p className="font-medium">Nenhuma transmissão aberta</p>
            <p className="text-sm text-muted-foreground">Cole um código acima para assistir. Dá para abrir várias ao mesmo tempo, e elas aparecem lado a lado.</p>
          </div>
        </div>
      ) : (
        <div
          className={cn("grid min-h-0 flex-1 gap-3", fullscreen ? "p-0" : "p-3 pt-0")}
          style={{ gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))`, gridTemplateRows: `repeat(${rows}, minmax(0, 1fr))` }}
        >
          {shown.map((v) => <Tile key={v.id} viewer={v} focused={focus === v.id} layoutKey={layoutKey} bare={fullscreen} />)}
        </div>
      )}
    </div>
  );
}

function AddBar({ count }: { count: number }) {
  const [code, setCode] = useState("");
  const focus = useApp((s) => s.focus);
  const submit = (c = code) => { if (c.trim()) { joinWithCode(c); setCode(""); } };
  const paste = async () => {
    try { const t = (await navigator.clipboard.readText()).trim(); if (t) submit(t); }
    catch { toast.error("Não deu para ler a área de transferência. Use Ctrl+V."); }
  };
  return (
    <div className="flex shrink-0 items-center gap-3 px-3 py-3">
      <h2 className="font-semibold">Assistindo</h2>
      {count > 0 && <Badge variant="secondary">{count}</Badge>}
      {focus && (
        <Button variant="outline" size="sm" onClick={() => setFocus(null)}><LayoutGrid />Ver todas</Button>
      )}
      <form className="ml-auto flex w-full max-w-md gap-2" onSubmit={(e) => { e.preventDefault(); submit(); }}>
        <Input value={code} onChange={(e) => setCode(e.target.value)} placeholder="Cole outro código (AJ2.)" spellCheck={false} autoComplete="off" className="font-mono" aria-label="Código da transmissão" />
        <Tooltip>
          <TooltipTrigger asChild>
            <Button type="button" variant="outline" size="icon" onClick={paste} aria-label="Colar e entrar"><ClipboardPaste /></Button>
          </TooltipTrigger>
          <TooltipContent>Colar e entrar</TooltipContent>
        </Tooltip>
        <Button type="submit"><Plus />Adicionar</Button>
      </form>
    </div>
  );
}

function Tile({ viewer: v, focused, layoutKey, bare }: { viewer: Viewer; focused: boolean; layoutKey: string; bare: boolean }) {
  const area = useRef<HTMLDivElement>(null);
  const [controls, setControls] = useState(true);
  const hideTimer = useRef<number>(undefined);

  // Em tela cheia, os controles somem sozinhos e voltam ao mexer o mouse.
  const bump = () => {
    setControls(true);
    clearTimeout(hideTimer.current);
    if (bare) hideTimer.current = window.setTimeout(() => setControls(false), 2500);
  };
  useEffect(() => { bump(); return () => clearTimeout(hideTimer.current); }, [bare]);
  useEffect(() => onPlayerInput((id, kind) => { if (id === v.id && kind === "move") bump(); }), [v.id, bare]);

  // O player nativo desta stream fica exatamente sobre a área de vídeo do tile.
  const showChrome = !bare || controls;
  useLayoutEffect(() => {
    const el = area.current;
    if (!el) return;
    const sync = () => setPlayerRect(v.id, el.getBoundingClientRect(), v.watching);
    sync();
    const raf = requestAnimationFrame(sync);
    const ro = new ResizeObserver(sync);
    ro.observe(el);
    window.addEventListener("resize", sync);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
      window.removeEventListener("resize", sync);
      setPlayerRect(v.id, null, false);
    };
  }, [v.id, v.watching, layoutKey, showChrome]);

  const stepIndex = v.step ? STEPS.findIndex((s) => s.id === v.step) : STEPS.length;
  const VolIcon = v.muted || v.volume === 0 ? VolumeX : v.volume < 50 ? Volume1 : Volume2;
  const toggleFocus = () => { setFocus(focused ? null : v.id); if (focused) setFullscreen(false); };

  return (
    <div className={cn("flex min-h-0 min-w-0 flex-col overflow-hidden bg-card", bare ? "" : "rounded-xl border")} onMouseMove={bump}>
      {showChrome && (
        <div className="flex shrink-0 items-center gap-2.5 border-b px-3 py-2">
          <Avatar className="size-7"><AvatarFallback className="text-[11px]">{initials(v.hostName || "?")}</AvatarFallback></Avatar>
          <div className="min-w-0 leading-tight">
            <p className="truncate text-sm font-medium">{v.hostName || "Conectando…"}</p>
            <p className={cn("text-xs", v.watching ? "text-live-text" : "text-muted-foreground")}>{v.watching ? "Ao vivo, P2P" : "Entrando"}</p>
          </div>
          {v.stats && v.watching && (
            <div className="ml-auto hidden flex-wrap justify-end gap-1 @container md:flex">
              <Badge variant="secondary" className="tabular-nums">{v.stats.height}p {Math.round(v.stats.fps)}fps</Badge>
              <Badge variant="secondary" className="tabular-nums">{formatRate(v.stats.kbps)}</Badge>
              {v.ping !== null && (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Badge variant="secondary" className={cn("tabular-nums", v.ping > 150 && "bg-warn text-black")}>
                      {v.ping} ms{v.stats.buffer_ms > 0 ? ` +${v.stats.buffer_ms}` : ""}
                    </Badge>
                  </TooltipTrigger>
                  <TooltipContent>Ping até o host{v.stats.buffer_ms > 0 ? `, mais ${v.stats.buffer_ms} ms de buffer para o vídeo não engasgar com a variação da rede` : ""}</TooltipContent>
                </Tooltip>
              )}
            </div>
          )}
          <div className={cn("flex gap-0.5", !(v.stats && v.watching) && "ml-auto")}>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button variant="ghost" size="icon-sm" onClick={toggleFocus} aria-label={focused ? "Voltar para a grade" : "Ampliar"}>
                  {focused ? <Minimize2 /> : <Maximize2 />}
                </Button>
              </TooltipTrigger>
              <TooltipContent>{focused ? "Voltar para a grade" : "Ampliar (clique duplo no vídeo para tela cheia)"}</TooltipContent>
            </Tooltip>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button variant="ghost" size="icon-sm" onClick={() => endWatching(v.id)} aria-label="Fechar"><X /></Button>
              </TooltipTrigger>
              <TooltipContent>Parar de assistir</TooltipContent>
            </Tooltip>
          </div>
        </div>
      )}

      <div ref={area} className="relative min-h-0 flex-1 bg-black" onDoubleClick={() => { setFocus(v.id); setFullscreen(!bare); }}>
        {!v.watching && (
          <div className="absolute inset-0 grid place-items-center bg-background p-4">
            <ol className="grid gap-2.5">
              {STEPS.map((s, i) => (
                <li key={s.id} className={cn("flex items-center gap-2.5 text-sm", i !== stepIndex && "text-muted-foreground")}>
                  <span className="grid size-4 place-items-center">
                    {i < stepIndex ? <Check className="size-4 text-live-text" />
                      : i === stepIndex ? <Loader2 className="size-4 animate-spin text-primary" />
                      : <span className="size-1.5 rounded-full bg-muted-foreground/40" />}
                  </span>
                  {s.label}
                </li>
              ))}
            </ol>
          </div>
        )}
      </div>

      {showChrome && v.watching && (
        <div className="flex shrink-0 items-center gap-2.5 border-t px-3 py-1.5">
          <Button variant="ghost" size="icon-sm" onClick={() => setViewerVolume(v.id, v.volume, !v.muted)} aria-label={v.muted ? "Ativar som" : "Silenciar"}>
            <VolIcon />
          </Button>
          <Slider className="w-32" min={0} max={100} step={1} value={[v.muted ? 0 : v.volume]} onValueChange={([x]) => setViewerVolume(v.id, x, false)} aria-label={`Volume de ${v.hostName}`} />
          <span className="w-9 text-xs tabular-nums text-muted-foreground">{v.muted ? 0 : v.volume}%</span>
        </div>
      )}
    </div>
  );
}
