import { useEffect, useState } from "react";
import { Eye, Radio, RefreshCw } from "lucide-react";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { joinWithCode, refreshPublic } from "@/engine";
import { initials } from "@/lib/rtc";
import { setState, useApp, type PublicStream } from "@/store";
import { cn } from "@/lib/utils";

const REFRESH_MS = 20_000;

function since(ms: number) {
  const min = Math.max(0, Math.floor((Date.now() - ms) / 60_000));
  if (min < 1) return "começou agora";
  if (min < 60) return `há ${min} min`;
  const h = Math.floor(min / 60);
  return `há ${h} h${min % 60 ? ` ${min % 60} min` : ""}`;
}

/** Quem está transmitindo publicamente agora (lido do mural na DHT, sem servidor). */
export function LiveDashboard() {
  const streams = useApp((s) => s.publicStreams);
  const loading = useApp((s) => s.publicLoading);
  const updated = useApp((s) => s.publicUpdated);
  const net = useApp((s) => s.net);
  const [, tick] = useState(0);

  useEffect(() => {
    if (net !== "ok") return;
    refreshPublic();
    const id = window.setInterval(refreshPublic, REFRESH_MS);
    const clock = window.setInterval(() => tick((n) => n + 1), 15_000);
    return () => { clearInterval(id); clearInterval(clock); };
  }, [net]);

  const others = streams?.filter((s) => !s.own) ?? [];
  const mine = streams?.find((s) => s.own);

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Radio className="size-4 text-red-500" />Ao vivo agora
          {streams && others.length > 0 && <Badge variant="secondary">{others.length}</Badge>}
        </CardTitle>
        <CardDescription>
          Transmissões públicas de qualquer pessoa usando o AntiJanja.
          {updated > 0 && ` Atualizado ${Math.round((Date.now() - updated) / 1000) < 5 ? "agora" : `há ${Math.round((Date.now() - updated) / 1000)} s`}.`}
        </CardDescription>
        <CardAction>
          <Button variant="ghost" size="sm" onClick={refreshPublic} disabled={loading || net !== "ok"}>
            <RefreshCw className={cn(loading && "animate-spin")} />Atualizar
          </Button>
        </CardAction>
      </CardHeader>
      <CardContent>
        {net !== "ok" ? (
          <p className="py-6 text-center text-sm text-muted-foreground">Esperando a conexão com a rede P2P.</p>
        ) : streams === null ? (
          <div className="grid gap-3 md:grid-cols-2">
            {[0, 1].map((i) => <Skeleton key={i} className="h-[76px] rounded-lg" />)}
          </div>
        ) : others.length === 0 ? (
          <p className="py-6 text-center text-sm text-muted-foreground">
            Ninguém transmitindo publicamente agora.
            {mine ? " A sua transmissão está na lista para os outros." : " Quando alguém começar, aparece aqui."}
          </p>
        ) : (
          <div className="grid gap-3 md:grid-cols-2">
            {others.map((s) => <StreamCard key={s.code} stream={s} />)}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function StreamCard({ stream: s }: { stream: PublicStream }) {
  const watchingThis = useApp((st) => st.viewers.some((v) => v.code === s.code));
  return (
    <div className="flex items-center gap-3 rounded-lg border p-3">
      <div className="relative">
        <Avatar className="size-10"><AvatarFallback>{initials(s.name)}</AvatarFallback></Avatar>
        <span className="absolute -right-0.5 -bottom-0.5 size-3 rounded-full border-2 border-card bg-red-500" />
      </div>
      <div className="min-w-0 flex-1">
        <p className="truncate font-medium">{s.name}</p>
        <p className="truncate text-sm text-muted-foreground">{s.title || "Tela"}</p>
        <div className="mt-1 flex flex-wrap gap-1.5 text-xs text-muted-foreground">
          <span className="flex items-center gap-1"><Eye className="size-3" />{s.viewers}</span>
          <span>{s.quality}</span>
          <span>{since(s.since)}</span>
        </div>
      </div>
      {watchingThis ? (
        <Button variant="secondary" size="sm" onClick={() => setState({ view: "watch" })}>Assistindo</Button>
      ) : (
        <Button size="sm" onClick={() => joinWithCode(s.code)}>Assistir</Button>
      )}
    </div>
  );
}
