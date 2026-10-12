import clsx from "clsx";
import { X } from "lucide-react";
import { motion, useReducedMotion } from "motion/react";
import { useId, useLayoutEffect, useRef, useState, type PointerEvent, type ReactNode, type SyntheticEvent } from "react";
import { createPortal } from "react-dom";
import { EASE_OUT, SPRING_IOS } from "./motion";

export interface DrawerProps {
  open: boolean;
  onClose: () => void;
  title: ReactNode;
  children: ReactNode;
  side?: "left" | "right";
  /** "sheet": centered card on desktop, bottom sheet on phones. */
  variant?: "drawer" | "sheet";
  closeLabel?: string;
  className?: string;
}

// Open modals scale and dim the page behind them (iOS card stacking); a
// counter keeps nested modals from clearing the effect early.
let openModals = 0;
function stackBackground(delta: 1 | -1) {
  openModals = Math.max(0, openModals + delta);
  const content = document.querySelector<HTMLElement>(".route-content");
  if (delta > 0 && openModals === 1 && content) content.style.setProperty("--modal-origin", `${window.innerHeight / 2 - content.getBoundingClientRect().top}px`);
  if (openModals) document.documentElement.dataset.modal = "";
  else delete document.documentElement.dataset.modal;
}

// The native modal dialog supplies focus containment and background inertness.
// Closing unmounts immediately, so authentication cleanup never waits for an exit animation.
function OpenDrawer({ onClose, title, children, side = "right", variant = "drawer", closeLabel = "关闭", className }: Omit<DrawerProps, "open">) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const closeRef = useRef<HTMLButtonElement>(null);
  const previousFocus = useRef<HTMLElement | null>(null);
  const titleId = useId();
  const reduceMotion = useReducedMotion();
  const [phone] = useState(() => window.matchMedia?.("(max-width: 767px)").matches ?? false);

  useLayoutEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog.showModal();
    stackBackground(1);
    closeRef.current?.focus();
    return () => {
      stackBackground(-1);
      if (dialog.open) dialog.close();
      const target = previousFocus.current;
      if (target?.isConnected) target.focus();
    };
  }, []);

  function handleCancel(event: SyntheticEvent<HTMLDialogElement>) {
    event.preventDefault();
    onClose();
  }

  function handleBackdrop(event: PointerEvent<HTMLDialogElement>) {
    if (event.target === event.currentTarget) onClose();
  }

  const sheet = variant === "sheet";
  const from = reduceMotion ? { opacity: 0 }
    : sheet ? phone ? { y: "100%" } : { opacity: 0, scale: 0.94 }
      : { x: side === "right" ? "100%" : "-100%" };
  const to = reduceMotion ? { opacity: 1 } : sheet ? phone ? { y: 0 } : { opacity: 1, scale: 1 } : { x: 0 };
  return createPortal(
    <dialog
      ref={dialogRef}
      aria-modal="true"
      aria-labelledby={titleId}
      className="beui-drawer"
      data-variant={variant}
      onCancel={handleCancel}
      onPointerDown={handleBackdrop}
    >
      <motion.div
        className={clsx("beui-drawer__panel", className)}
        data-side={side}
        data-variant={variant}
        initial={from}
        animate={to}
        transition={reduceMotion ? { duration: 0.15 } : sheet ? SPRING_IOS : { duration: 0.21, ease: EASE_OUT }}
      >
        {sheet && <span className="beui-drawer__grabber" aria-hidden="true" />}
        <header className="beui-drawer__header">
          <h2 id={titleId} className="beui-drawer__title">{title}</h2>
          <button ref={closeRef} type="button" className="beui-drawer__close" aria-label={closeLabel} onClick={onClose}>
            <X size={18} aria-hidden="true" />
          </button>
        </header>
        <div className="beui-drawer__content">{children}</div>
      </motion.div>
    </dialog>,
    document.body,
  );
}

// Based on beUI Drawer. Native <dialog> replaces the upstream overlay's
// incomplete focus handling while keeping its slide-in Motion treatment.
export function Drawer({ open, ...props }: DrawerProps) {
  return open ? <OpenDrawer {...props} /> : null;
}
