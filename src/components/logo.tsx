export function Logo({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 32 32" className={className} aria-hidden="true">
      <rect x="4" y="7" width="17" height="12" rx="2.5" fill="none" stroke="currentColor" strokeWidth="2.4" />
      <rect x="11" y="13" width="17" height="12" rx="2.5" fill="currentColor" />
      <circle cx="7" cy="25" r="2.6" className="fill-red-500" />
    </svg>
  );
}
