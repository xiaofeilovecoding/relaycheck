import { ReactNode, useEffect, useRef } from "react";

// Relay buckle: two open rounded rings meeting on a horizontal stroke — an
// original mark (spec §8), rendered from the frozen design geometry.
export function BuckleMark({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 28 28" fill="none" aria-hidden="true">
      <path
        d="M15 6H9a6 6 0 0 0 0 12h4M13 10h6a6 6 0 0 1 0 12h-4"
        stroke="currentColor"
        strokeWidth="2.5"
        strokeLinecap="round"
      />
      <path d="m10 14 8 0" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" />
    </svg>
  );
}

export function Modal({
  title,
  onClose,
  children,
  initialFocusRef,
}: {
  title: string;
  onClose: () => void;
  children: ReactNode;
  initialFocusRef?: React.RefObject<HTMLElement>;
}) {
  const panelRef = useRef<HTMLDivElement>(null);
  const restoreRef = useRef<HTMLElement | null>(null);

  useEffect(() => {
    restoreRef.current = document.activeElement as HTMLElement | null;
    const panel = panelRef.current;
    const focusTarget = initialFocusRef?.current ?? panel;
    focusTarget?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
      if (e.key === "Tab" && panel) {
        // Simple focus trap.
        const focusables = panel.querySelectorAll<HTMLElement>(
          'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
        );
        if (focusables.length === 0) return;
        const first = focusables[0];
        const last = focusables[focusables.length - 1];
        if (e.shiftKey && document.activeElement === first) {
          e.preventDefault();
          last.focus();
        } else if (!e.shiftKey && document.activeElement === last) {
          e.preventDefault();
          first.focus();
        }
      }
    };
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("keydown", onKey);
      restoreRef.current?.focus();
    };
  }, [onClose, initialFocusRef]);

  return (
    <div className="backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div
        className="modal"
        ref={panelRef}
        role="dialog"
        aria-modal="true"
        aria-label={title}
        tabIndex={-1}
      >
        {children}
      </div>
    </div>
  );
}

export function ErrorNote({ error, details }: { error?: string | null; details?: string[] }) {
  if (!error) return null;
  return (
    <div className="error-note" role="alert">
      {error}
      {details && details.length > 0 ? (
        <ul>
          {details.slice(0, 12).map((d, i) => (
            <li key={i}>{d}</li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}
