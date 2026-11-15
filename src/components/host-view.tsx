import { Copy, TriangleAlert, Cpu, Eye, Gauge, RefreshCw, Repeat, Square, UserX } from "lucide-react";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Slider } from "@/components/ui/slider";
import { Switch } from "@/components/ui/switch";
import { Globe, Lock } from "lucide-react";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { AudioSource } from "@/components/audio-source";
import { copyCode, kickPeer, newCode, openPicker, stopSharing, updateSettings } from "@/engine";
import { formatRate, initials } from "@/lib/rtc";
import { useApp } from "@/store";

export function HostView() {
  const source = useApp((s) => s.source);
  const preview = useApp((s) => s.preview);
  const info = useApp((s) => s.info);
  const viewers = useApp((s) => s.peers.filter((p) => p.state !== "connecting").length);

  return (
    <div className="h-full overflow-y-auto">
      <div className="grid gap-6 p-6 lg:grid-cols-[minmax(0,1fr)_340px]">
        <div className="flex min-w-0 flex-col gap-4">
          <div className="flex items-center gap-3">
            <div className="min-w-0">
              <h2 className="flex items-center gap-2 text-lg font-semibold">
                <span className="size-2 animate-pulse rounded-full bg-red-500" />Você está transmitindo
              </h2>
              <p className="truncate text-sm text-muted-foreground">{source?.name}</p>
            </div>
            <div className="ml-auto flex gap-2">
              <Button variant="outline" onClick={() => openPicker("switch")}><Repeat />Trocar fonte</Button>
              <Button variant="destructive" onClick={() => stopSharing()}><Square />Parar</Button>
            </div>
          </div>

          <div className="relative aspect-video max-h-[60vh] overflow-hidden rounded-xl border bg-black">
            {preview && <img src={preview} alt="Prévia do que está sendo transmitido" className="absolute inset-0 size-full object-contain" />}
            <Badge variant="secondary" className="absolute top-3 left-3 bg-background/80 backdrop-blur">Prévia, atualiza a cada segundo</Badge>
            <Badge variant="secondary" className="absolute top-3 right-3 gap-1.5 bg-background/80 backdrop-blur">
              <Eye className="size-3.5" />{viewers}
            </Badge>
            {info && (
              <div className="pointer-events-none absolute right-3 bottom-3 left-3 flex flex-wrap gap-1.5">
                <StatBadge label="Envio" value={`${info.width}×${info.height}`} />
                <StatBadge label="FPS" value={String(Math.round(info.fps))} />
                <StatBadge label="Taxa" value={`${formatRate(info.kbps)} de ${formatRate(info.target_kbps)}`} />
              </div>
            )}
          </div>

          <SettingsCard />
        </div>

        <div className="flex flex-col gap-6">
          <CodeCard />
          <ViewersCard />
        </div>
      </div>
    </div>
  );
}

function StatBadge({ label, value }: { label: string; value: string }) {
  return (
    <Badge variant="secondary" className="gap-1.5 bg-background/80 tabular-nums backdrop-blur">
      <span className="text-muted-foreground">{label}</span>{value}
    </Badge>
  );
}

/* ---------------------------------------------------------------- Código */

function CodeCard() {
  const code = useApp((s) => s.code);
  const isPublic = useApp((s) => s.settings.public);
  const groups = code ? code.slice(4).match(/.{1,4}/g) ?? [] : [];
  return (
    <Card>
      <CardHeader>
        <CardTitle>Código da transmissão</CardTitle>
        <CardDescription>Quem tiver o código entra sozinho. Você não precisa aprovar.</CardDescription>
        <CardAction>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button variant="ghost" size="icon-sm" disabled={!code} onClick={newCode} aria-label="Gerar outro código"><RefreshCw /></Button>
            </TooltipTrigger>
            <TooltipContent>Gerar outro código. Quem já está assistindo continua.</TooltipContent>
          </Tooltip>
        </CardAction>
      </CardHeader>
      <CardContent>
        <div className="min-h-24 rounded-lg border border-dashed bg-muted/40 p-4 font-mono text-[15px] leading-relaxed select-all">
          {code ? (
            <span className="flex flex-wrap gap-x-2.5 gap-y-1">
              <span className="font-semibold text-primary">{code.slice(0, 4)}</span>
              {groups.map((g, i) => <span key={i}>{g}</span>)}
            </span>
          ) : (
            <span className="font-sans text-sm text-muted-foreground">Gerando código…</span>
          )}
        </div>
      </CardContent>
      <CardFooter className="flex-col items-stretch gap-3">
        <Button className="w-full" disabled={!code} onClick={() => copyCode(code)}><Copy />Copiar código</Button>
        <div className="flex items-center justify-between gap-3 rounded-lg border px-3 py-2.5">
          <div className="flex items-center gap-2.5">
            {isPublic ? <Globe className="size-4 text-emerald-500" /> : <Lock className="size-4 text-muted-foreground" />}
            <div className="grid leading-tight">
              <Label htmlFor="public-live" className="text-sm">{isPublic ? "Pública" : "Só com o código"}</Label>
              <span className="text-xs text-muted-foreground">{isPublic ? "Aparece em “Ao vivo agora”" : "Não aparece na lista"}</span>
            </div>
          </div>
          <Switch id="public-live" checked={isPublic} onCheckedChange={(v) => updateSettings({ public: v })} />
        </div>
      </CardFooter>
    </Card>
  );
}

/* ----------------------------------------------------------- Espectadores */

const STATE_TEXT = { connecting: "Entrando", connected: "Assistindo agora", unstable: "Conexão instável" };

function ViewersCard() {
  const peers = useApp((s) => s.peers);
  return (
    <Card>
      <CardHeader>
        <CardTitle>Assistindo</CardTitle>
        {!peers.length && <CardDescription>Ninguém ainda. Quem entrar com o código aparece aqui.</CardDescription>}
      </CardHeader>
      {peers.length > 0 && (
        <CardContent className="grid gap-1">
          {peers.map((p) => (
            <div key={p.id} className="group flex items-center gap-3 rounded-lg px-2 py-1.5 hover:bg-muted/50">
              <Avatar className="size-8"><AvatarFallback className="text-xs">{initials(p.name)}</AvatarFallback></Avatar>
              <div className="min-w-0 flex-1">
                <p className="truncate text-sm font-medium">{p.name}</p>
                <p className={`text-xs ${p.state === "connected" ? "text-emerald-500" : "text-amber-500"}`}>{STATE_TEXT[p.state]}</p>
              </div>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button variant="ghost" size="icon-sm" className="opacity-0 group-hover:opacity-100 focus-visible:opacity-100" onClick={() => kickPeer(p.id)} aria-label={`Remover ${p.name}`}>
                    <UserX />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>Remover da transmissão</TooltipContent>
              </Tooltip>
            </div>
          ))}
        </CardContent>
      )}
    </Card>
  );
}

/* --------------------------------------------------------- Configurações */

const RES = [["0", "Original"], ["2160", "2160p (4K)"], ["1440", "1440p"], ["1080", "1080p"], ["900", "900p"], ["720", "720p"], ["480", "480p"]];
const FPS = [["30", "30"], ["60", "60"], ["120", "120"], ["144", "144"]];
const BITRATE = [["0", "Automático"], ["3000", "3 Mb/s"], ["6000", "6 Mb/s"], ["10000", "10 Mb/s"], ["15000", "15 Mb/s"], ["20000", "20 Mb/s"], ["30000", "30 Mb/s"], ["50000", "50 Mb/s"], ["80000", "80 Mb/s"]];
const ABITRATE = [["64", "64 kb/s"], ["96", "96 kb/s"], ["128", "128 kb/s"], ["192", "192 kb/s"], ["256", "256 kb/s"], ["320", "320 kb/s"], ["510", "510 kb/s (máximo)"]];

function SettingsCard() {
  const s = useApp((st) => st.settings);
  const info = useApp((st) => st.info);
  const audioStatus = useApp((st) => st.audioStatus);
  const level = useApp((st) => st.audioLevel);
  const watchingCount = useApp((st) => st.viewers.length);

  return (
    <Card>
      <CardHeader>
        <CardTitle>Configurações da transmissão</CardTitle>
        <CardDescription>As mudanças valem na hora, para todo mundo que está assistindo.</CardDescription>
      </CardHeader>
      <CardContent>
        <Tabs defaultValue="video">
          <TabsList>
            <TabsTrigger value="video">Vídeo</TabsTrigger>
            <TabsTrigger value="audio">Áudio</TabsTrigger>
          </TabsList>

          <TabsContent value="video" className="grid gap-4 pt-3">
            <div className="grid grid-cols-3 gap-4">
              <Field label="Resolução" value={String(s.res)} options={RES} onChange={(v) => updateSettings({ res: Number(v) })} />
              <Field label="Quadros por segundo" value={String(s.fps)} options={FPS} onChange={(v) => updateSettings({ fps: Number(v) })} />
              <Field label="Bitrate alvo" value={String(s.bitrate)} options={BITRATE} onChange={(v) => updateSettings({ bitrate: Number(v) })} />
            </div>
            <p className="-mt-2 text-xs text-muted-foreground">
              O bitrate é atingido com movimento na tela. Com a imagem parada, o encoder gasta menos, porque não há o que mandar.
            </p>
            <div className="flex items-center justify-between gap-4 rounded-lg border px-4 py-3">
              <div className="grid gap-0.5">
                <Label htmlFor="cursor">Mostrar o cursor do mouse</Label>
                <p className="text-xs text-muted-foreground">Desligue para transmitir só o conteúdo.</p>
              </div>
              <Switch id="cursor" checked={s.cursor} onCheckedChange={(v) => updateSettings({ cursor: v })} />
            </div>
            {info && (
              <div className="grid gap-1 rounded-lg bg-muted/40 px-4 py-3 text-sm">
                <p className="flex items-center gap-2">
                  {info.hardware ? <Gauge className="size-4 text-emerald-500" /> : <Cpu className="size-4 text-amber-500" />}
                  H.264 com {info.encoder}{info.hardware ? ", na placa de vídeo" : ", no processador"}
                </p>
                <p className="text-xs text-muted-foreground">
                  {info.zero_copy ? "A imagem não sai da placa de vídeo (sem cópia para a CPU)." : "A imagem passa pela CPU antes de codificar."}
                  {" "}{info.encode_ms.toFixed(1).replace(".", ",")} ms por quadro. O Windows entregou {Math.round(info.captured_fps)} quadros novos por segundo; o resto são repetições de imagem parada.
                </p>
              </div>
            )}
          </TabsContent>

          <TabsContent value="audio" className="grid gap-4 pt-3">
            <AudioSource />
            {watchingCount > 0 && (s.audioMode === "exclude" || s.audioMode === "include") && (
              <p className="flex items-start gap-2 rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-200">
                <TriangleAlert className="mt-0.5 size-4 shrink-0" />
                {s.audioMode === "exclude"
                  ? "O áudio das transmissões que você está assistindo também vai junto neste modo. Use “Todo o PC”, que deixa o AntiJanja de fora."
                  : "Neste modo só o app escolhido vai na transmissão, então o áudio do que você assiste não vaza."}
              </p>
            )}
            <div className="grid grid-cols-2 gap-4">
              <Field label="Bitrate do áudio" value={String(s.abitrate)} options={ABITRATE} onChange={(v) => updateSettings({ abitrate: Number(v) })} />
              <div className="grid gap-2">
                <div className="flex items-center justify-between">
                  <Label>Volume</Label>
                  <span className="text-sm tabular-nums text-muted-foreground">{s.volume}%</span>
                </div>
                <Slider className="my-auto" min={0} max={200} step={5} value={[s.volume]} onValueChange={([v]) => updateSettings({ volume: v })} aria-label="Volume" />
              </div>
            </div>
            <div className="flex items-center gap-3 text-sm">
              <div className="h-1.5 w-24 shrink-0 overflow-hidden rounded-full bg-muted">
                <div className="h-full rounded-full bg-emerald-500 transition-[width] duration-100" style={{ width: `${Math.min(100, level * 100)}%` }} />
              </div>
              <span className={audioStatus.kind === "err" ? "text-destructive" : audioStatus.kind === "ok" ? "text-foreground" : "text-muted-foreground"}>
                {audioStatus.text}
              </span>
            </div>
          </TabsContent>
        </Tabs>
      </CardContent>
    </Card>
  );
}

function Field({ label, value, options, onChange }: { label: string; value: string; options: string[][]; onChange: (v: string) => void }) {
  return (
    <div className="grid min-w-0 gap-2">
      <Label>{label}</Label>
      <Select value={value} onValueChange={onChange}>
        <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
        <SelectContent>
          {options.map(([v, l]) => <SelectItem key={v} value={v}>{l}</SelectItem>)}
        </SelectContent>
      </Select>
    </div>
  );
}
