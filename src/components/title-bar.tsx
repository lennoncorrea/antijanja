import { Minus, Square, X } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { appWindow, closeApp } from "@/engine";
import { setState, useApp, type View } from "@/store";
import { Logo } from "@/components/logo";
import { cn } from "@/lib/utils";

export function TitleBar() {
  const view = useApp((s) => s.view);
  const live = useApp((s) => s.live);
  const watchingCount = useApp((s) => s.viewers.length);
  const viewers = useApp((s) => s.peers.filter((p) => p.state !== "connecting").length);

  const tabs: { id: View; label: React.ReactNode; show: boolean }[] = [
    { id: "home", label: "Início", show: true },
    {
      id: "host",
      label: (
        <span className="flex items-center gap-1.5">
          <span className="size-1.5 animate-pulse rounded-full bg-red-500" />
          Transmitindo{viewers ? <Badge variant="secondary" className="h-4 px-1.5 text-[10px]">{viewers}</Badge> : null}
        </span>
      ),
      show: live,
    },
    {
      id: "watch",
      label: (
        <span className="flex items-center gap-1.5">
          Assistindo{watchingCount ? <Badge variant="secondary" className="h-4 px-1.5 text-[10px]">{watchingCount}</Badge> : null}
        </span>
      ),
      show: true,
    },
  ];

  return (
    <header data-tauri-drag-region className="flex h-10 shrink-0 items-center gap-3 border-b pl-3">
      <Logo className="pointer-events-none size-5" />
      <span data-tauri-drag-region className="text-sm font-semibold tracking-tight">AntiJanja</span>

      <nav className="ml-3 flex h-full items-center gap-1">
        {tabs.filter((t) => t.show).map((t) => (
          <button
            key={t.id}
            onClick={() => setState({ view: t.id })}
            className={cn(
              "h-7 rounded-md px-2.5 text-sm text-muted-foreground transition-colors hover:bg-muted hover:text-foreground",
              view === t.id && "bg-muted font-medium text-foreground",
            )}
          >
            {t.label}
          </button>
        ))}
      </nav>

      <div className="ml-auto flex h-full">
        <WinButton label="Minimizar" onClick={() => appWindow.minimize()}><Minus className="size-4" /></WinButton>
        <WinButton label="Maximizar" onClick={() => appWindow.toggleMaximize()}><Square className="size-3.5" /></WinButton>
        <WinButton label="Fechar" onClick={closeApp} danger><X className="size-4" /></WinButton>
      </div>
    </header>
  );
}

function WinButton({ label, onClick, danger, children }: { label: string; onClick: () => void; danger?: boolean; children: React.ReactNode }) {
  return (
    <button
      aria-label={label}
      onClick={onClick}
      className={`grid h-full w-11 place-items-center text-muted-foreground transition-colors hover:text-foreground ${danger ? "hover:bg-red-600 hover:text-white" : "hover:bg-muted"}`}
    >
      {children}
    </button>
  );
}
