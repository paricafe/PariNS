import { motion, useReducedMotion, type HTMLMotionProps } from "motion/react";
import clsx from "clsx";
import { forwardRef, type ReactNode } from "react";
import { SPRING_PRESS } from "./motion";

export type ButtonVariant = "primary" | "secondary" | "ghost" | "outline";
export type ButtonSize = "sm" | "md" | "lg" | "icon";
export interface ButtonProps extends Omit<HTMLMotionProps<"button">, "children"> {
  variant?: ButtonVariant;
  size?: ButtonSize;
  children?: ReactNode;
}

// Based on beUI Button/base.tsx. Demo ripple and magnetic effects are omitted.
export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  { variant = "primary", size = "md", className, children, disabled, ...props },
  ref,
) {
  const reduceMotion = useReducedMotion();
  return (
    <motion.button
      ref={ref}
      type="button"
      disabled={disabled}
      whileTap={disabled || reduceMotion ? undefined : { scale: 0.98 }}
      transition={SPRING_PRESS}
      className={clsx("beui-button", `beui-button--${variant}`, `beui-button--${size}`, className)}
      {...props}
    >
      {children}
    </motion.button>
  );
});
