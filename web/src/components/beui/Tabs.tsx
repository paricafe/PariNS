import clsx from "clsx";
import { motion, useReducedMotion } from "motion/react";
import { useId, useRef, type KeyboardEvent, type ReactNode } from "react";
import { SPRING_LAYOUT } from "./motion";

export interface TabItem {
  value: string;
  label: ReactNode;
  disabled?: boolean;
}

export interface TabsProps {
  items: readonly TabItem[];
  value: string;
  onChange: (value: string) => void;
  label?: string;
  className?: string;
}

// Based on beUI Tabs' moving underline; the adapted API is fully controlled.
export function Tabs({ items, value, onChange, label, className }: TabsProps) {
  const indicatorId = useId();
  const buttons = useRef<Array<HTMLButtonElement | null>>([]);
  const reduceMotion = useReducedMotion();

  function onKeyDown(event: KeyboardEvent<HTMLButtonElement>, index: number) {
    const enabled = items.map((item, position) => item.disabled ? -1 : position).filter((position) => position >= 0);
    if (!enabled.length) return;
    const current = enabled.indexOf(index);
    let next: number;
    switch (event.key) {
      case "ArrowRight": next = enabled[(current + 1) % enabled.length]; break;
      case "ArrowLeft": next = enabled[(current - 1 + enabled.length) % enabled.length]; break;
      case "Home": next = enabled[0]; break;
      case "End": next = enabled[enabled.length - 1]; break;
      default: return;
    }
    event.preventDefault();
    buttons.current[next]?.focus();
    onChange(items[next].value);
  }

  return (
    <div role="tablist" aria-label={label} className={clsx("beui-tabs", className)}>
      {items.map((item, index) => {
        const selected = item.value === value;
        return (
          <button
            key={item.value}
            ref={(node) => { buttons.current[index] = node; }}
            type="button"
            role="tab"
            aria-selected={selected}
            tabIndex={selected && !item.disabled ? 0 : -1}
            disabled={item.disabled}
            className="beui-tab"
            onClick={() => onChange(item.value)}
            onKeyDown={(event) => onKeyDown(event, index)}
          >
            {item.label}
            {selected && (
              <motion.span
                aria-hidden="true"
                layoutId={indicatorId}
                className="beui-tab__indicator"
                transition={reduceMotion ? { duration: 0 } : SPRING_LAYOUT}
              />
            )}
          </button>
        );
      })}
    </div>
  );
}
