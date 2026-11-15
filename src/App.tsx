import { useEffect } from "react";
import { Toaster } from "@/components/ui/sonner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { TitleBar } from "@/components/title-bar";
import { HomeView } from "@/components/home-view";
import { HostView } from "@/components/host-view";
import { WatchView } from "@/components/watch-view";
import { SourcePicker } from "@/components/source-picker";
import { UpdateOverlay } from "@/components/update-overlay";
import { initEngine } from "@/engine";
import { useApp } from "@/store";

export default function App() {
  const view = useApp((s) => s.view);
  const fullscreen = useApp((s) => s.fullscreen);

  useEffect(() => { initEngine(); }, []);

  return (
    <TooltipProvider delayDuration={300}>
      <div className="flex h-screen flex-col overflow-hidden bg-background text-foreground select-none">
        {!fullscreen && <TitleBar />}
        <main className="relative min-h-0 flex-1">
          {view === "home" && <HomeView />}
          {view === "host" && <HostView />}
          {view === "watch" && <WatchView />}
        </main>
      </div>
      <SourcePicker />
      <UpdateOverlay />
      <Toaster position="top-center" richColors offset={52} />
    </TooltipProvider>
  );
}
