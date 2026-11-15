import { useMemo, useState } from "react";
import { AppWindow, Monitor, RefreshCw } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { AudioSource } from "@/components/audio-source";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { closePicker, loadSources, startSharing, switchSource, updateSettings } from "@/engine";
import { useApp, type Source } from "@/store";
import { cn } from "@/lib/utils";

export function SourcePicker() {
  const mode = useApp((s) => s.picker);
  const sources = useApp((s) => s.sources);
  const current = useApp((s) => s.source);
  const isPublic = useApp((s) => s.settings.public);
  const [tab, setTab] = useState<"monitor" | "window">("monitor");
  const [selected, setSelected] = useState<string | null>(null);

  const list = useMemo(() => (sources ?? []).filter((s) => s.kind === tab), [sources, tab]);
  const chosen = sources?.find((s) => s.id === selected) ?? null;
  const counts = {
    monitor: sources?.filter((s) => s.kind === "monitor").length ?? 0,
    window: sources?.filter((s) => s.kind === "window").length ?? 0,
  };

  const confirm = (src: Source | null = chosen) => {
    if (!src) return;
    setSelected(null);
    if (mode === "switch") switchSource(src); else startSharing(src);
  };

  return (
    <Dialog open={mode !== null} onOpenChange={(open) => { if (!open) { closePicker(); setSelected(null); } }}>
      <DialogContent className="flex max-h-[88vh] flex-col gap-4 sm:max-w-4xl">
        <DialogHeader>
          <DialogTitle>{mode === "switch" ? "Trocar o que está sendo transmitido" : "O que você quer transmitir?"}</DialogTitle>
          <DialogDescription>
            {mode === "switch"
              ? "Quem está assistindo continua conectado, com o mesmo código."
              : "Escolha uma tela inteira ou só uma janela."}
          </DialogDescription>
        </DialogHeader>

        <div className="flex items-center justify-between gap-3">
          <Tabs value={tab} onValueChange={(v) => setTab(v as "monitor" | "window")}>
            <TabsList>
              <TabsTrigger value="monitor"><Monitor />Telas{sources && <span className="text-muted-foreground">{counts.monitor}</span>}</TabsTrigger>
              <TabsTrigger value="window"><AppWindow />Janelas{sources && <span className="text-muted-foreground">{counts.window}</span>}</TabsTrigger>
            </TabsList>
          </Tabs>
          <Button variant="ghost" size="sm" onClick={loadSources} disabled={!sources}>
            <RefreshCw className={cn(!sources && "animate-spin")} />Atualizar
          </Button>
        </div>

        {/* Rolagem nativa: o ScrollArea do Radix não limita a altura dentro de um flex com max-h,
            e as miniaturas vazavam por cima do resto do diálogo. */}
        <div className="-mx-1 min-h-40 flex-1 overflow-y-auto overscroll-contain">
          <div className={cn("grid gap-3 px-1 py-1", tab === "monitor" && list.length <= 2 ? "grid-cols-2" : "grid-cols-2 md:grid-cols-3")}>
            {!sources &&
              Array.from({ length: tab === "monitor" ? 2 : 6 }).map((_, i) => (
                <div key={i} className="grid gap-2">
                  <Skeleton className="aspect-video w-full rounded-lg" />
                  <Skeleton className="h-4 w-2/3" />
                </div>
              ))}
            {sources && list.length === 0 && (
              <p className="col-span-full py-10 text-center text-sm text-muted-foreground">
                {tab === "window" ? "Nenhuma janela aberta para transmitir. Janelas minimizadas não aparecem." : "Nenhuma tela encontrada."}
              </p>
            )}
            {list.map((s) => {
              const isSelected = selected === s.id;
              const isCurrent = mode === "switch" && current?.id === s.id;
              return (
                <button
                  key={s.id}
                  onClick={() => setSelected(s.id)}
                  onDoubleClick={() => confirm(s)}
                  className={cn(
                    "group grid gap-2 rounded-xl border bg-card p-2 text-left transition-colors outline-none hover:border-foreground/30 focus-visible:ring-2 focus-visible:ring-ring",
                    isSelected && "border-primary ring-2 ring-primary/40 hover:border-primary",
                  )}
                >
                  <div className="relative aspect-video overflow-hidden rounded-lg bg-muted">
                    {s.thumbnail ? (
                      <img src={s.thumbnail} alt="" className="size-full object-contain" draggable={false} />
                    ) : (
                      <div className="grid size-full place-items-center text-muted-foreground">
                        {s.kind === "monitor" ? <Monitor className="size-8" /> : <AppWindow className="size-8" />}
                      </div>
                    )}
                    <div className="absolute top-2 left-2 flex gap-1">
                      {s.primary && <Badge variant="secondary" className="bg-background/80 backdrop-blur">Principal</Badge>}
                      {isCurrent && <Badge className="bg-live text-live-foreground">No ar</Badge>}
                    </div>
                  </div>
                  <div className="min-w-0 px-1">
                    <p className="truncate text-sm font-medium">{s.name}</p>
                    <p className="truncate text-xs text-muted-foreground">
                      {s.kind === "window" && s.app ? `${s.app}, ` : ""}{s.width}×{s.height}
                    </p>
                  </div>
                </button>
              );
            })}
          </div>
        </div>

        {mode === "start" && (
          <>
            <Separator />
            <div className="grid gap-2">
              <p className="text-sm font-medium">Áudio que vai junto</p>
              <AudioSource />
            </div>
            <div className="flex items-center justify-between gap-4 rounded-lg border px-4 py-3">
              <div className="grid gap-0.5">
                <Label htmlFor="public-start">Aparecer em “Ao vivo agora”</Label>
                <p className="text-xs text-muted-foreground">Qualquer pessoa com o AntiJanja vê sua transmissão na lista e pode entrar. Desligado, só entra quem receber o código.</p>
              </div>
              <Switch id="public-start" checked={isPublic} onCheckedChange={(v) => updateSettings({ public: v })} />
            </div>
          </>
        )}

        <DialogFooter>
          <Button variant="ghost" onClick={() => { closePicker(); setSelected(null); }}>Cancelar</Button>
          <Button onClick={() => confirm()} disabled={!chosen}>
            {mode === "switch" ? "Trocar para esta fonte" : "Começar transmissão"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
