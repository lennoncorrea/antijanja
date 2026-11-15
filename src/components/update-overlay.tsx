import { useEffect, useState } from "react";
import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { Download } from "lucide-react";

type Status = { version: string; done: number; total: number | null };

let started = false; // o StrictMode monta duas vezes em desenvolvimento

/**
 * Ao abrir, procura uma versão nova nas releases do GitHub. Se houver, baixa, instala e
 * reinicia sozinho, com uma tela de progresso por cima de tudo. Sem internet ou com
 * erro, o app segue normalmente.
 */
export function UpdateOverlay() {
  const [status, setStatus] = useState<Status | null>(null);

  useEffect(() => {
    if (started || import.meta.env.DEV) return;
    started = true;
    (async () => {
      try {
        const update = await check({ timeout: 10_000 });
        if (!update) return;
        setStatus({ version: update.version, done: 0, total: null });
        let done = 0;
        await update.downloadAndInstall((ev) => {
          if (ev.event === "Started") setStatus({ version: update.version, done: 0, total: ev.data.contentLength ?? null });
          else if (ev.event === "Progress") {
            done += ev.data.chunkLength;
            setStatus((s) => s && { ...s, done });
          }
        });
        // No Windows o instalador fecha o app sozinho; isto cobre os outros casos.
        await relaunch();
      } catch (e) {
        console.warn("Atualização automática falhou:", e);
        setStatus(null);
      }
    })();
  }, []);

  if (!status) return null;
  const pct = status.total ? Math.min(100, Math.round((status.done / status.total) * 100)) : null;
  return (
    <div className="fixed inset-0 z-[100] grid place-items-center bg-background/95 backdrop-blur">
      <div className="grid w-80 gap-4 text-center">
        <Download className="mx-auto size-8 text-primary" />
        <div className="grid gap-1">
          <p className="font-medium">Atualizando para a versão {status.version}</p>
          <p className="text-sm text-muted-foreground">O AntiJanja reinicia sozinho quando terminar.</p>
        </div>
        <div className="h-1.5 overflow-hidden rounded-full bg-muted">
          <div
            className={pct === null ? "h-full w-1/3 animate-pulse rounded-full bg-primary" : "h-full rounded-full bg-primary transition-[width]"}
            style={pct === null ? undefined : { width: `${pct}%` }}
          />
        </div>
        {pct !== null && <p className="text-xs tabular-nums text-muted-foreground">{pct}%</p>}
      </div>
    </div>
  );
}
