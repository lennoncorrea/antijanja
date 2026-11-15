import { AppWindow, RefreshCw, Volume2, VolumeOff, VolumeX } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { appName, refreshApps, updateSettings } from "@/engine";
import { useApp, type AudioMode } from "@/store";
import { cn } from "@/lib/utils";

const MODES: { value: AudioMode; label: string; hint: string; icon: typeof Volume2 }[] = [
  { value: "none", label: "Sem áudio", hint: "Só a imagem", icon: VolumeOff },
  { value: "system", label: "Todo o PC", hint: "Tudo que toca aqui", icon: Volume2 },
  { value: "exclude", label: "Tudo, menos um app", hint: "Por exemplo, sem o Discord", icon: VolumeX },
  { value: "include", label: "Só um app", hint: "Por exemplo, só o jogo", icon: AppWindow },
];

/** Escolha da fonte de áudio. `detailed` mostra a descrição de cada opção. */
export function AudioSource({ detailed = false }: { detailed?: boolean }) {
  const mode = useApp((s) => s.settings.audioMode);
  const app = useApp((s) => s.settings.audioApp);
  const apps = useApp((s) => s.apps);
  const needsApp = mode === "exclude" || mode === "include";

  const options = [...apps];
  if (app && !options.some((a) => a.exe === app)) options.push({ exe: app, name: `${appName(app)} (fechado)`, active: false });

  return (
    <div className="grid gap-3">
      <ToggleGroup
        type="single"
        variant="outline"
        value={mode ?? ""}
        onValueChange={(v) => {
          if (!v) return;
          const patch: { audioMode: AudioMode; audioApp?: string } = { audioMode: v as AudioMode };
          if ((v === "exclude" || v === "include") && !app) patch.audioApp = apps[0]?.exe || "";
          updateSettings(patch);
        }}
        className={cn("grid w-full gap-2", detailed ? "grid-cols-2" : "grid-cols-2 xl:grid-cols-4")}
        spacing={2}
      >
        {MODES.map(({ value, label, hint, icon: Icon }) => (
          <ToggleGroupItem
            key={value}
            value={value}
            className={cn("h-auto justify-start gap-3 rounded-lg px-3 data-[state=on]:border-primary", detailed ? "py-3" : "py-2")}
          >
            <Icon className="size-4 shrink-0" />
            <span className="flex flex-col items-start text-left">
              <span className="font-medium">{label}</span>
              {detailed && <span className="text-xs font-normal text-muted-foreground">{hint}</span>}
            </span>
          </ToggleGroupItem>
        ))}
      </ToggleGroup>

      {needsApp && (
        <div className="flex gap-2">
          <Select
            value={app || undefined}
            onValueChange={(v) => updateSettings({ audioApp: v })}
          >
            <SelectTrigger className="w-full" aria-label="Aplicativo">
              <SelectValue placeholder={options.length ? "Escolha um aplicativo" : "Nenhum app com áudio encontrado"} />
            </SelectTrigger>
            <SelectContent>
              {options.map((a) => (
                <SelectItem key={a.exe} value={a.exe}>
                  {a.name}
                  {a.active && <span className="text-xs text-live-text">tocando agora</span>}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Button variant="outline" size="icon" onClick={refreshApps} aria-label="Atualizar lista de apps">
            <RefreshCw />
          </Button>
        </div>
      )}
    </div>
  );
}
