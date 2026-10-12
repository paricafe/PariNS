import clsx from "clsx";
import { motion, useReducedMotion } from "motion/react";
import { useId, useState } from "react";
import { SPRING_IOS_FAST } from "./motion";

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

// Based on beUI Switch. iOS-style: the thumb stretches toward the travel
// direction while pressed and springs across on release.
export function Switch({ checked, onCheckedChange, disabled, label, id, className, ...aria }: SwitchProps) {
  const generatedId = useId();
  const controlId = id ?? generatedId;
  const reduceMotion = useReducedMotion();
  const [pressed, setPressed] = useState(false);
  const stretched = pressed && !disabled && !reduceMotion;
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
        onPointerDown={() => setPressed(true)}
        onPointerUp={() => setPressed(false)}
        onPointerLeave={() => setPressed(false)}
        onPointerCancel={() => setPressed(false)}
      >
        <motion.span
          aria-hidden="true"
          className="beui-switch__thumb"
          initial={false}
          style={{ transformOrigin: checked ? "right center" : "left center" }}
          animate={{ x: checked ? 20 : 0, scaleX: stretched ? 1.22 : 1 }}
          transition={reduceMotion ? { duration: 0 } : SPRING_IOS_FAST}
        />
      </button>
      {label && <label className="beui-switch__label" htmlFor={controlId}>{label}</label>}
    </span>
  );
}
