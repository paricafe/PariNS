import clsx from "clsx";
import { motion, useReducedMotion } from "motion/react";
import { useId } from "react";
import { SPRING_LAYOUT } from "./motion";

export interface SwitchProps {
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
  disabled?: boolean;
  label?: string;
  id?: string;
  "aria-label"?: string;
  "aria-labelledby"?: string;
  "aria-describedby"?: string;
  className?: string;
}

// Based on beUI Switch, without its disabled-state shake or pressed stretch.
export function Switch({ checked, onCheckedChange, disabled, label, id, className, ...aria }: SwitchProps) {
  const generatedId = useId();
  const controlId = id ?? generatedId;
  const reduceMotion = useReducedMotion();
  return (
    <span className={clsx("beui-switch-wrap", className)}>
      <button
        id={controlId}
        type="button"
        role="switch"
        aria-checked={checked}
        aria-label={aria["aria-label"]}
        aria-labelledby={aria["aria-labelledby"]}
        aria-describedby={aria["aria-describedby"]}
        disabled={disabled}
        className="beui-switch"
        data-state={checked ? "checked" : "unchecked"}
        onClick={() => onCheckedChange(!checked)}
      >
        <motion.span
          aria-hidden="true"
          className="beui-switch__thumb"
          initial={false}
          animate={{ x: checked ? 20 : 0 }}
          transition={reduceMotion ? { duration: 0 } : SPRING_LAYOUT}
        />
      </button>
      {label && <label className="beui-switch__label" htmlFor={controlId}>{label}</label>}
    </span>
  );
}
