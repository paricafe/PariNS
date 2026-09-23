import { translate, type Language } from '../i18n';
import type { TransportChange } from '../session/client';

export function transportChangeText(change: TransportChange, language: Language): string {
  if (!change.next_origin) return translate('app.transportNextUnknown', language);
  return translate(change.to === 'https' ? 'app.transportNextHttps' : 'app.transportNextHttp', language,
    { origin: change.next_origin });
}

export function TransportHint({ change, language, link = false }: { change: TransportChange; language: Language; link?: boolean }) {
  return <div className="notice" role="status">
    <p>{transportChangeText(change, language)}</p>
    {link && change.next_origin && <a href={`${change.next_origin}${window.location.pathname}${window.location.hash}`}>
      {translate('app.openNewAddress', language)} · {change.next_origin}
    </a>}
  </div>;
}
