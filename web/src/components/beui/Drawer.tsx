import clsx from "clsx";
import { X } from "lucide-react";
import { motion, useReducedMotion } from "motion/react";
import { useId, useLayoutEffect, useRef, type PointerEvent, type ReactNode, type SyntheticEvent } from "react";
import { createPortal } from "react-dom";
import { EASE_OUT } from "./motion";

export interface DrawerProps {
  open: boolean;
  onClose: () => void;
  title: ReactNode;
  children: ReactNode;
  side?: "left" | "right";
  closeLabel?: string;
  className?: string;
}

// The native modal dialog supplies focus containment and background inertness.
// Closing unmounts immediately, so authentication cleanup never waits for an exit animation.
function OpenDrawer({ onClose, title, children, side = "right", closeLabel = "关闭", className }: Omit<DrawerProps, "open">) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const closeRef = useRef<HTMLButtonElement>(null);
  const previousFocus = useRef<HTMLElement | null>(null);
  const titleId = useId();
  const reduceMotion = useReducedMotion();

  useLayoutEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog.showModal();
    closeRef.current?.focus();
    return () => {
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

  return createPortal(
    <dialog
      ref={dialogRef}
      aria-modal="true"
      aria-labelledby={titleId}
      className="beui-drawer"
      onCancel={handleCancel}
      onPointerDown={handleBackdrop}
    >
      <motion.div
        className={clsx("beui-drawer__panel", className)}
        data-side={side}
        initial={reduceMotion ? { opacity: 0 } : { x: side === "right" ? "100%" : "-100%" }}
        animate={reduceMotion ? { opacity: 1 } : { x: 0 }}
        transition={reduceMotion ? { duration: 0 } : { duration: 0.21, ease: EASE_OUT }}
      >
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
