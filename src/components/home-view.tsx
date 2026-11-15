import { useEffect, useState } from "react";
import { ClipboardPaste, Eye, Play, Radio, RefreshCw } from "lucide-react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { copyCode, joinWithCode, openPicker, refreshPublic, updateSettings } from "@/engine";
import { initials } from "@/lib/rtc";
import { save, setState, useApp, type PublicStream, type Viewer } from "@/store";
import { cn } from "@/lib/utils";

const REFRESH_MS = 20_000;

export function HomeView() {
  return (
    <div className="flex h-full">
      <main className="min-w-0 flex-1 overflow-y-auto">
        <LiveNow />
      </main>
      <aside className="flex w-[300px] shrink-0 flex-col gap-[18px] overflow-y-auto border-l bg-rail px-5 py-6">
        <TransmitCard />
        <CodeEntry />
        <WatchingList />
        <NameField />
      </aside>
    </div>
  );
}

/* ------------------------------------------------------------ utilidades */

function Eyebrow({ children, className, ...props }: React.ComponentProps<"span">) {
  return <span className={cn("font-mono text-[11px] font-medium tracking-[0.1em] text-muted-foreground uppercase", className)} {...props}>{children}</span>;
}

function since(ms: number) {
  const min = Math.max(0, Math.floor((Date.now() - ms) / 60_000));
  if (min < 1) return "começou agora";
  if (min < 60) return `há ${min} min`;
  const h = Math.floor(min / 60);
  return `há ${h} h${min % 60 ? ` ${min % 60} min` : ""}`;
}

/** Cor da capa a partir do nome: sempre a mesma para a mesma pessoa. */
function coverIndex(name: string) {
  let h = 0;
  for (const c of name) h = (h * 31 + c.charCodeAt(0)) | 0;
  return (Math.abs(h) % 3) + 1;
}

function Cover({ name, className, children, big }: { name: string; className?: string; children?: React.ReactNode; big?: boolean }) {
  const n = coverIndex(name);
  return (
    <div
      className={cn("scanlines relative flex items-center justify-center overflow-hidden", className)}
      style={big ? { background: "#0f2e12", color: "#4dc75b" } : { background: `var(--cover-${n})`, color: `var(--cover-${n}-ink)` }}
    >
      <span className={cn("font-extrabold tracking-[-0.04em] select-none", big ? "text-8xl" : "text-[40px]")}>{initials(name)}</span>
      {children}
    </div>
  );
}

function MediaTag({ children, className }: { children: React.ReactNode; className?: string }) {
  return <span className={cn("absolute rounded bg-[#09200a]/85 px-2 py-1 font-mono text-xs font-medium text-[#f8fcf9]", className)}>{children}</span>;
}

/* --------------------------------------------------------- Ao vivo agora */

function LiveNow() {
  const streams = useApp((s) => s.publicStreams);
  const loading = useApp((s) => s.publicLoading);
  const updated = useApp((s) => s.publicUpdated);
  const net = useApp((s) => s.net);
  const [, tick] = useState(0);

  useEffect(() => {
    if (net !== "ok") return;
    refreshPublic();
    const id = window.setInterval(refreshPublic, REFRESH_MS);
    const clock = window.setInterval(() => tick((n) => n + 1), 5_000);
    return () => { clearInterval(id); clearInterval(clock); };
  }, [net]);

  const others = streams?.filter((s) => !s.own) ?? [];
  const mine = streams?.some((s) => s.own) ?? false;
  const watching = others.reduce((n, s) => n + s.viewers, 0);
  const ago = updated ? Math.round((Date.now() - updated) / 1000) : null;
  const [featured, ...rest] = others;

  return (
    <div className="flex flex-col gap-5 px-7 pt-7 pb-6">
      <div className="flex flex-wrap items-end gap-x-4 gap-y-2">
        <div className="flex flex-col gap-1.5">
          <span className="flex items-center gap-2 font-mono text-[11px] font-medium tracking-[0.12em] text-live-text">
            <span className="size-2 rounded-full bg-live shadow-[0_0_0_4px_color-mix(in_oklab,var(--live)_22%,transparent)]" />NO AR
          </span>
          <h1 className="text-[34px] leading-none font-extrabold tracking-[-0.03em]">Ao vivo agora</h1>
        </div>
        {streams && net === "ok" && (
          <span className="pb-0.5 font-mono text-[13px] text-muted-foreground">
            {others.length} {others.length === 1 ? "transmissão" : "transmissões"} · {watching} assistindo
          </span>
        )}
        <Tooltip>
          <TooltipTrigger asChild>
            <Button variant="outline" size="sm" className="ml-auto font-normal text-muted-foreground" onClick={refreshPublic} disabled={loading || net !== "ok"}>
              <RefreshCw className={cn(loading && "animate-spin")} />
              {loading ? "Atualizando" : ago === null ? "Atualizar" : ago < 5 ? "agora" : `há ${ago} s`}
            </Button>
          </TooltipTrigger>
          <TooltipContent>Atualiza sozinho a cada 20 s</TooltipContent>
        </Tooltip>
      </div>

      {net !== "ok" ? (
        <Empty
          title={net === "err" ? "Sem acesso à rede P2P" : "Entrando na rede P2P…"}
          text={net === "err" ? "Confira sua internet. A lista de quem está ao vivo vem dessa rede." : "Leva alguns segundos. Depois a lista aparece aqui."}
        />
      ) : streams === null ? (
        <>
          <Skeleton className="h-[236px] rounded-[14px]" />
          <div className="grid grid-cols-2 gap-3 xl:grid-cols-3">
            {[0, 1, 2].map((i) => <Skeleton key={i} className="h-40 rounded-xl" />)}
          </div>
        </>
      ) : !featured ? (
        <Empty
          title="Ninguém transmitindo publicamente agora"
          text={mine ? "A sua transmissão está na lista para os outros." : "Quando alguém começar uma transmissão pública, ela aparece aqui."}
          action={!mine && <Button onClick={() => openPicker("start")}><Radio />Seja o primeiro</Button>}
        />
      ) : (
        <>
          <Featured stream={featured} />
          {rest.length > 0 && (
            <div className="grid grid-cols-2 gap-3 xl:grid-cols-3">
              {rest.map((s) => <StreamCard key={s.code} stream={s} />)}
            </div>
          )}
        </>
      )}
    </div>
  );
}

function Empty({ title, text, action }: { title: string; text: string; action?: React.ReactNode }) {
  return (
    <div className="flex flex-col items-center gap-3 rounded-[14px] border border-dashed bg-card/60 px-6 py-14 text-center">
      <span className="grid size-12 place-items-center rounded-full bg-muted text-muted-foreground"><Radio className="size-5" /></span>
      <p className="text-base font-semibold">{title}</p>
      <p className="max-w-sm text-sm text-muted-foreground">{text}</p>
      {action}
    </div>
  );
}

function useWatching(code: string) {
  return useApp((st) => st.viewers.some((v) => v.code === code));
}

function Featured({ stream: s }: { stream: PublicStream }) {
  const watching = useWatching(s.code);
  return (
    <div className="flex overflow-hidden rounded-[14px] border bg-card">
      <Cover name={s.name} big className="aspect-video w-[44%] max-w-[420px] shrink-0">
        <span className="absolute top-3 left-3 rounded bg-live px-2 py-1 font-mono text-[11px] font-bold tracking-[0.08em] text-live-foreground">AO VIVO</span>
        <MediaTag className="bottom-3 left-3">{s.quality}</MediaTag>
      </Cover>
      <div className="flex min-w-0 flex-1 flex-col gap-2.5 px-6 py-5">
        <Eyebrow>Mais assistida</Eyebrow>
        <h2 className="truncate text-[26px] leading-tight font-bold tracking-[-0.02em]">{s.name}</h2>
        <p className="truncate text-[15px] text-foreground/80">{s.title || "Tela"}</p>
        <div className="flex flex-wrap gap-x-4 gap-y-1 font-mono text-xs text-muted-foreground">
          <span className="flex items-center gap-1.5"><Eye className="size-3.5" />{s.viewers} assistindo</span>
          <span>{since(s.since)}</span>
        </div>
        <div className="mt-auto flex flex-wrap gap-2 pt-2">
          {watching ? (
            <Button size="lg" variant="secondary" onClick={() => setState({ view: "watch" })}>Assistindo</Button>
          ) : (
            <Button size="lg" className="font-bold" onClick={() => joinWithCode(s.code)}><Play className="fill-current" />Assistir</Button>
          )}
          <Button size="lg" variant="outline" onClick={() => copyCode(s.code)}>Copiar código</Button>
        </div>
      </div>
    </div>
  );
}

function StreamCard({ stream: s }: { stream: PublicStream }) {
  const watching = useWatching(s.code);
  return (
    <div className="flex flex-col overflow-hidden rounded-xl border bg-card">
      <Cover name={s.name} className="h-[104px]">
        <span className="absolute top-2 left-2 size-2 rounded-full bg-live" />
        <MediaTag className="right-2 bottom-2 px-1.5 py-0.5 text-[11px]">{s.quality}</MediaTag>
      </Cover>
      <div className="flex items-center gap-2.5 px-3 pt-2.5 pb-3">
        <div className="min-w-0 flex-1">
          <p className="truncate text-sm font-semibold">{s.name}</p>
          <p className="truncate text-xs text-muted-foreground">{s.title || "Tela"}</p>
          <p className="mt-1 flex items-center gap-1 font-mono text-[11px] text-muted-foreground"><Eye className="size-3" />{s.viewers} · {since(s.since)}</p>
        </div>
        {watching ? (
          <Button variant="secondary" size="sm" onClick={() => setState({ view: "watch" })}>Assistindo</Button>
        ) : (
          <Tooltip>
            <TooltipTrigger asChild>
              <Button variant="secondary" size="icon" aria-label={`Assistir ${s.name}`} onClick={() => joinWithCode(s.code)}>
                <Play className="size-3 fill-current" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>Assistir</TooltipContent>
          </Tooltip>
        )}
      </div>
    </div>
  );
}

/* ------------------------------------------------------------ Coluna */

function TransmitCard() {
  const live = useApp((s) => s.live);
  const isPublic = useApp((s) => s.settings.public);
  const viewers = useApp((s) => s.peers.filter((p) => p.state !== "connecting").length);
  return (
    <div className="flex flex-col gap-3 rounded-[14px] border bg-card p-[18px]">
      <Eyebrow>Transmitir</Eyebrow>
      {live ? (
        <>
          <p className="flex items-center gap-2 text-[19px] leading-tight font-bold tracking-[-0.02em]">
            <span className="size-2 animate-pulse rounded-full bg-live" />Você está no ar
          </p>
          <p className="text-sm text-muted-foreground">{viewers === 0 ? "Ninguém assistindo ainda." : viewers === 1 ? "1 pessoa assistindo." : `${viewers} pessoas assistindo.`}</p>
          <Button size="lg" className="h-11 bg-strong text-strong-foreground hover:bg-strong/90" onClick={() => setState({ view: "host" })}>Ver minha transmissão</Button>
        </>
      ) : (
        <>
          <p className="text-[19px] leading-tight font-bold tracking-[-0.02em]">Sua tela no ar em 2 cliques</p>
          <Button size="lg" className="h-11 bg-strong text-strong-foreground hover:bg-strong/90" onClick={() => openPicker("start")}>
            <span className="size-2 rounded-full bg-live" />Começar transmissão
          </Button>
        </>
      )}
      <div className="flex items-center justify-between gap-3">
        <Label htmlFor="home-public" className="text-[13px] font-normal text-foreground/80">Aparecer em “Ao vivo”</Label>
        <Switch id="home-public" checked={isPublic} onCheckedChange={(v) => updateSettings({ public: v })} />
      </div>
    </div>
  );
}

function CodeEntry() {
  const [code, setCode] = useState("");
  const paste = async () => {
    try {
      const text = (await navigator.clipboard.readText()).trim();
      setCode(text);
      if (text) joinWithCode(text);
    } catch {
      toast.error("Não deu para ler a área de transferência. Use Ctrl+V.");
    }
  };
  return (
    <form className="flex flex-col gap-2" onSubmit={(e) => { e.preventDefault(); joinWithCode(code); }}>
      <Eyebrow><label htmlFor="home-code">Entrar com código</label></Eyebrow>
      <div className="flex gap-1.5">
        <Input
          id="home-code"
          value={code}
          onChange={(e) => setCode(e.target.value)}
          placeholder="AJ2.…"
          spellCheck={false}
          autoComplete="off"
          className="h-10 bg-card font-mono text-[13px]"
        />
        <Tooltip>
          <TooltipTrigger asChild>
            <Button type="button" variant="secondary" size="icon" className="size-10 shrink-0 border" onClick={paste} aria-label="Colar e entrar"><ClipboardPaste /></Button>
          </TooltipTrigger>
          <TooltipContent>Colar e entrar</TooltipContent>
        </Tooltip>
      </div>
    </form>
  );
}

const STEP_TEXT = { prepare: "Preparando", publish: "Procurando o host", wait: "Esperando o host", connect: "Conectando" };

function WatchingList() {
  const viewers = useApp((s) => s.viewers);
  if (!viewers.length) return null;
  return (
    <div className="flex flex-col gap-2">
      <Eyebrow>Você está assistindo</Eyebrow>
      {viewers.map((v) => <WatchingRow key={v.id} v={v} />)}
    </div>
  );
}

function WatchingRow({ v }: { v: Viewer }) {
  const name = v.hostName || "Conectando…";
  const detail = v.watching ? (v.ping !== null ? `ao vivo · ${v.ping} ms` : "ao vivo") : v.step ? STEP_TEXT[v.step] : "";
  return (
    <button
      onClick={() => setState({ view: "watch" })}
      className="flex items-center gap-2.5 rounded-[10px] border bg-card p-2.5 text-left transition-colors outline-none hover:border-foreground/25 focus-visible:ring-2 focus-visible:ring-ring"
    >
      <Cover name={v.hostName || v.code} className="size-[34px] shrink-0 rounded-lg [&>span:first-child]:text-[13px] [&>span:first-child]:tracking-normal" />
      <div className="min-w-0 flex-1">
        <p className="truncate text-[13px] font-semibold">{name}</p>
        <p className="truncate text-xs text-muted-foreground">{detail}</p>
      </div>
    </button>
  );
}

function NameField() {
  const name = useApp((s) => s.name);
  return (
    <div className="mt-auto flex items-center gap-2.5 border-t pt-3.5">
      <span className="grid size-8 shrink-0 place-items-center rounded-full bg-muted text-xs font-semibold">{initials(name || "?")}</span>
      <div className="min-w-0 flex-1">
        <label htmlFor="my-name" className="block text-xs text-muted-foreground">Seu nome</label>
        <input
          id="my-name"
          value={name}
          maxLength={40}
          spellCheck={false}
          placeholder="Como te chamam"
          onChange={(e) => { setState({ name: e.target.value }); save("name", e.target.value); }}
          className="-mx-1 w-full rounded-md bg-transparent px-1 py-0.5 text-[13px] font-semibold outline-none hover:bg-muted focus-visible:bg-muted focus-visible:ring-2 focus-visible:ring-ring"
        />
      </div>
    </div>
  );
}
