import { useEffect, useRef, useState } from 'react';
import { translate, type Language } from '../../i18n';

interface TurnstileApi {
  render(container: HTMLElement, options: Record<string, unknown>): string;
  reset(widget: string): void;
  remove(widget: string): void;
}
declare global { interface Window { turnstile?: TurnstileApi } }

// The server allows this script and its frame only on a sign-in document that shows the widget.
const SCRIPT = 'https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit';
let loading: Promise<TurnstileApi> | null = null;

function load(): Promise<TurnstileApi> {
  if (window.turnstile) return Promise.resolve(window.turnstile);
  loading ??= new Promise<TurnstileApi>((resolve, reject) => {
    const script = document.createElement('script');
    script.src = SCRIPT;
    script.async = true;
    script.onload = () => window.turnstile ? resolve(window.turnstile) : reject(new Error('Turnstile unavailable'));
    script.onerror = () => { loading = null; script.remove(); reject(new Error('Turnstile unavailable')); };
    document.head.appendChild(script);
  });
  return loading;
}

/** The Cloudflare Turnstile widget. Tokens are single-use: bump `resetSignal` after each attempt. */
export function TurnstileWidget({ siteKey, action, language, resetSignal, onToken }: {
  siteKey: string; action: string; language: Language; resetSignal: number; onToken: (token: string | null) => void;
}) {
  const container = useRef<HTMLDivElement>(null);
  const widget = useRef<{ api: TurnstileApi; id: string } | null>(null);
  const report = useRef(onToken);
  report.current = onToken;
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    let cancelled = false;
    setFailed(false);
    load().then((api) => {
      if (cancelled || !container.current) return;
      const id = api.render(container.current, {
        sitekey: siteKey, action, theme: 'auto', language: language === 'en' ? 'en' : 'zh-cn',
        callback: (token: string) => report.current(token),
        'expired-callback': () => report.current(null),
        'error-callback': () => report.current(null),
      });
      widget.current = { api, id };
    }).catch(() => { if (!cancelled) setFailed(true); });
    return () => {
      cancelled = true;
      if (widget.current) widget.current.api.remove(widget.current.id);
      widget.current = null;
      report.current(null);
    };
  }, [siteKey, action, language]);
  useEffect(() => {
    if (!resetSignal || !widget.current) return;
    widget.current.api.reset(widget.current.id);
    report.current(null);
  }, [resetSignal]);
  return failed
    ? <p className="notice error" role="alert">{translate('turnstile.widgetFailed', language)}</p>
    : <div ref={container} className="turnstile-widget" />;
}
