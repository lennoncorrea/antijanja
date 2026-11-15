import { useState } from "react";
import { ClipboardPaste, Eye, Monitor, ShieldCheck } from "lucide-react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { joinWithCode, openPicker } from "@/engine";
import { LiveDashboard } from "@/components/live-dashboard";
import { setState as setAppState } from "@/store";
import { save, setState, useApp } from "@/store";

export function HomeView() {
  const name = useApp((s) => s.name);
  const net = useApp((s) => s.net);
  const live = useApp((s) => s.live);
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
    <div className="h-full overflow-y-auto">
      <div className="mx-auto flex max-w-4xl flex-col gap-8 px-8 py-12">
        <div className="space-y-3">
          <h1 className="text-4xl font-semibold tracking-tight">Compartilhe sua tela direto com quem você quiser</h1>
          <p className="max-w-2xl text-muted-foreground">
            Sem conta e sem servidor no meio. Você gera um código, a outra pessoa cola e a imagem vai de um computador para o outro.
          </p>
        </div>

        <div className="grid gap-4 md:grid-cols-2">
          <Card>
            <CardHeader>
              <CardTitle className="flex items-center gap-2"><Monitor className="size-4" />Transmitir</CardTitle>
              <CardDescription>Escolha uma tela ou janela. A imagem é capturada e codificada pela sua placa de vídeo.</CardDescription>
            </CardHeader>
            <CardFooter className="mt-auto">
              {live ? (
                <Button size="lg" variant="secondary" className="w-full" onClick={() => setAppState({ view: "host" })}>
                  <Monitor />Ver minha transmissão
                </Button>
              ) : (
                <Button size="lg" className="w-full" onClick={() => openPicker("start")}>
                  <Monitor />Começar transmissão
                </Button>
              )}
            </CardFooter>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="flex items-center gap-2"><Eye className="size-4" />Assistir</CardTitle>
              <CardDescription>Cole o código que você recebeu. Dá para assistir várias ao mesmo tempo, e transmitir junto.</CardDescription>
            </CardHeader>
            <CardContent>
              <form
                className="flex gap-2"
                onSubmit={(e) => { e.preventDefault(); joinWithCode(code); }}
              >
                <Input
                  value={code}
                  onChange={(e) => setCode(e.target.value)}
                  placeholder="AJ2."
                  spellCheck={false}
                  autoComplete="off"
                  aria-label="Código da transmissão"
                  className="font-mono"
                />
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button type="button" variant="outline" size="icon" onClick={paste} aria-label="Colar"><ClipboardPaste /></Button>
                  </TooltipTrigger>
                  <TooltipContent>Colar e entrar</TooltipContent>
                </Tooltip>
              </form>
            </CardContent>
            <CardFooter className="mt-auto">
              <Button size="lg" variant="secondary" className="w-full" onClick={() => joinWithCode(code)}>
                <Eye />Entrar
              </Button>
            </CardFooter>
          </Card>
        </div>

        <LiveDashboard />

        <div className="flex flex-wrap items-end justify-between gap-4 border-t pt-6">
          <div className="grid w-64 gap-2">
            <Label htmlFor="my-name">Seu nome</Label>
            <Input
              id="my-name"
              value={name}
              maxLength={40}
              spellCheck={false}
              onChange={(e) => { setState({ name: e.target.value }); save("name", e.target.value); }}
            />
          </div>
          <Tooltip>
            <TooltipTrigger asChild>
              <p className="flex cursor-help items-center gap-2 text-sm text-muted-foreground">
                <span className={`size-2 rounded-full ${net === "ok" ? "bg-emerald-500" : net === "err" ? "bg-red-500" : "animate-pulse bg-amber-500"}`} />
                {net === "ok" ? "Conectado à rede P2P" : net === "err" ? "Sem acesso à rede P2P. Confira sua internet." : "Entrando na rede P2P"}
                <ShieldCheck className="size-4" />
              </p>
            </TooltipTrigger>
            <TooltipContent className="max-w-xs">
              Os computadores se encontram pela DHT do BitTorrent, uma rede pública e descentralizada. As mensagens trocadas lá são criptografadas com o código.
            </TooltipContent>
          </Tooltip>
        </div>
      </div>

    </div>
  );
}
