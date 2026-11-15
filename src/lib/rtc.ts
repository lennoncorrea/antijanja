// Utilitários de texto usados pela interface.

export const errMsg = (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : String(e));

export const initials = (n?: string) =>
  (n || "?").trim().split(/\s+/).slice(0, 2).map((w) => w[0]).join("").toUpperCase() || "?";

export function formatRate(kbps: number) {
  return kbps >= 1000 ? `${(kbps / 1000).toFixed(1)} Mb/s` : `${Math.round(kbps)} kb/s`;
}
