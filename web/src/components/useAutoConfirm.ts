import { useCallback, useEffect, useRef, useState } from 'react';

/**
 * `checking`: reading the server to learn the outcome; `unresolved`: the reads worked but
 * do not show the outcome yet; `failed`: the reads themselves failed. The last two offer a
 * manual check.
 */
export type ConfirmState = 'checking' | 'unresolved' | 'failed' | null;

const wait = (ms: number) => new Promise<void>((resolve) => { window.setTimeout(resolve, ms); });

/**
 * Confirms an action whose response was lost by reading the server's state, never by
 * sending the action again. While `active`, `read` runs at once and then up to `attempts`
 * times, `delay` ms apart; it returns the outcome, or `undefined` while it is not visible.
 */
export function useAutoConfirm<T>(active: boolean, read: () => Promise<T | undefined>, settle: (outcome: T) => void, { attempts = 4, delay = 2000 } = {}) {
  const [state, setState] = useState<ConfirmState>(null);
  const owner = useRef(0);
  const reader = useRef(read); reader.current = read;
  const settler = useRef(settle); settler.current = settle;
  const run = useCallback(async (tries: number) => {
    const token = ++owner.current;
    setState('checking');
    let failed = false;
    for (let attempt = 0; attempt < tries; attempt += 1) {
      if (attempt > 0) await wait(delay);
      if (token !== owner.current) return;
      try {
        const outcome = await reader.current();
        if (token !== owner.current) return;
        failed = false;
        if (outcome !== undefined) { setState(null); settler.current(outcome); return; }
      } catch {
        if (token !== owner.current) return;
        failed = true;
      }
    }
    setState(failed ? 'failed' : 'unresolved');
  }, [delay]);
  useEffect(() => {
    if (active) void run(attempts);
    else { owner.current += 1; setState(null); }
    return () => { owner.current += 1; };
  }, [active, attempts, run]);
  /** One more read, for the manual button. */
  const check = useCallback(() => { void run(1); }, [run]);
  return { state, check };
}
