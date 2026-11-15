import { Minus, Moon, Square, Sun, X } from "lucide-react";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { appWindow, closeApp } from "@/engine";
import { setState, setTheme, useApp, type View } from "@/store";
import { Logo } from "@/components/logo";
import { cn } from "@/lib/utils";

export function TitleBar() {
  const view = useApp((s) => s.view);
  const live = useApp((s) => s.live);
  const theme = useApp((s) => s.theme);
  const watchingCount = useApp((s) => s.viewers.length);
  const viewers = useApp((s) => s.peers.filter((p) => p.state !== "connecting").length);

  const tabs: { id: View; label: React.ReactNode; show: boolean }[] = [
    { id: "home", label: "Início", show: true },
    {
      id: "host",
      label: (
        <span className="flex items-center gap-1.5">
          <span className="size-1.5 animate-pulse rounded-full bg-live" />
          Transmitindo{viewers ? <Count n={viewers} /> : null}
        </span>
      ),
      show: live,
    },
    {
      id: "watch",
      label: <span className="flex items-center gap-1.5">Assistindo{watchingCount ? <Count n={watchingCount} /> : null}</span>,
      show: true,
    },
  ];

  const dark = theme === "dark";

  return (
    <header data-tauri-drag-region className="flex h-11 shrink-0 items-center gap-3 border-b pl-4">
      <Logo className="pointer-events-none size-[22px]" />
      <span data-tauri-drag-region className="text-[15px] font-extrabold tracking-tight">AntiJanja</span>

      <nav className="ml-4 flex h-full items-center gap-1">
        {tabs.filter((t) => t.show).map((t) => (
          <button
            key={t.id}
            onClick={() => setState({ view: t.id })}
            className={cn(
              "h-[30px] rounded-md px-3 text-[13px] font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-foreground",
              view === t.id && "bg-muted font-semibold text-foreground",
            )}
          >
            {t.label}
          </button>
        ))}
      </nav>

      <div data-tauri-drag-region className="ml-auto flex h-full items-center gap-1">
        <NetStatus />
        <Tooltip>
          <TooltipTrigger asChild>
            <button
              onClick={() => setTheme(dark ? "light" : "dark")}
              aria-label={dark ? "Usar tema claro" : "Usar tema escuro"}
              className="grid size-8 place-items-center rounded-md text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
            >
              {dark ? <Sun className="size-4" /> : <Moon className="size-4" />}
            </button>
          </TooltipTrigger>
          <TooltipContent>{dark ? "Tema claro" : "Tema escuro"}</TooltipContent>
        </Tooltip>
      </div>

      <div className="flex h-full">
        <WinButton label="Minimizar" onClick={() => appWindow.minimize()}><Minus className="size-4" /></WinButton>
        <WinButton label="Maximizar" onClick={() => appWindow.toggleMaximize()}><Square className="size-3.5" /></WinButton>
        <WinButton label="Fechar" onClick={closeApp} danger><X className="size-4" /></WinButton>
      </div>
    </header>
  );
}

function Count({ n }: { n: number }) {
  return <span className="rounded bg-muted px-1.5 font-mono text-[11px] text-foreground/80">{n}</span>;
}

function NetStatus() {
  const net = useApp((s) => s.net);
  const text = net === "ok" ? "Rede P2P conectada" : net === "err" ? "Sem rede P2P" : "Entrando na rede P2P";
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="mr-1 flex cursor-help items-center gap-2 px-2 text-xs text-muted-foreground">
          <span className={cn("size-[7px] rounded-full", net === "ok" ? "bg-live" : net === "err" ? "bg-destructive" : "animate-pulse bg-warn")} />
          {text}
        </span>
      </TooltipTrigger>
      <TooltipContent className="max-w-xs">
        {net === "err"
          ? "Sem acesso à rede P2P. Confira sua internet."
          : "Os computadores se encontram pela DHT do BitTorrent, uma rede pública e descentralizada. As mensagens trocadas lá são criptografadas com o código."}
      </TooltipContent>
    </Tooltip>
  );
}

function WinButton({ label, onClick, danger, children }: { label: string; onClick: () => void; danger?: boolean; children: React.ReactNode }) {
  return (
    <button
      aria-label={label}
      onClick={onClick}
      className={cn(
        "grid h-full w-[46px] place-items-center text-muted-foreground transition-colors hover:text-foreground",
        danger ? "hover:bg-[#e81123] hover:text-white" : "hover:bg-muted",
      )}
    >
      {children}
    </button>
  );
}
