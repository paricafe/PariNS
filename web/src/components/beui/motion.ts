// Adapted from beUI's shared motion tokens (MIT; see README.md in this directory).
export const EASE_OUT = [0.16, 1, 0.3, 1] as const;
export const SPRING_PRESS = { type: "spring", stiffness: 500, damping: 30, mass: 0.6 } as const;
export const SPRING_LAYOUT = { type: "spring", stiffness: 360, damping: 32, mass: 0.6 } as const;
// Console motion scale: micro-interactions, entrances and number tweens.
export const DURATION = { micro: 0.18, enter: 0.45, count: 1 } as const;
export const SPRING_SOFT = { type: "spring", stiffness: 260, damping: 26, mass: 0.8 } as const;
