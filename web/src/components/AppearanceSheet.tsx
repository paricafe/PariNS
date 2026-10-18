import { useId, useRef, useState } from 'react';
import { ImagePlus, Palette, Trash2 } from 'lucide-react';
import { translate } from '../i18n';
import { MAX_OVERLAY, PATTERN_BACKGROUNDS, SOLID_BACKGROUNDS, type BackgroundId } from '../appearance/preferences';
import { ACCEPTED_TYPES, ImageError } from '../appearance/image';
import type { Appearance } from '../appearance/useAppearance';
import { Drawer } from './beui';

const PROBLEMS = { type: 'errorType', size: 'errorSize', decode: 'errorDecode', encode: 'errorEncode' } as const;

function Swatch({ id, name, checked, onSelect, label, image }: { id: BackgroundId; name: string; checked: boolean; onSelect(): void; label: string; image?: string }) {
  return <label className="bg-option" data-checked={checked || undefined}>
    <input type="radio" className="sr-only" name={name} value={id} checked={checked} onChange={onSelect} />
    <span className="bg-swatch" data-bg={id} aria-hidden="true" style={image ? { backgroundImage: `url("${image}")` } : undefined}>
      {id === 'orbs' && <><i /><i /></>}
    </span>
    <span className="bg-option-label">{label}</span>
  </label>;
}

/** Sheet for the background preset/pattern and the browser-local custom image; the top-bar button owns the theme. */
export function AppearanceSheet({ appearance }: { appearance: Appearance }) {
  const { language, background, overlay, image } = appearance;
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<{ key: string; tone: 'error' | 'info' } | null>(null);
  const file = useRef<HTMLInputElement>(null);
  const name = useId();
  const t = (key: string, params?: Record<string, string | number>) => translate(`appearance.${key}`, language, params);
  const option = (id: BackgroundId) => <Swatch key={id} id={id} name={name} checked={background === id} label={t(`bg_${id}`)} onSelect={() => appearance.changeBackground(id)} />;
  const upload = async (picked: File | undefined) => {
    if (!picked) return;
    setBusy(true); setMessage(null);
    try {
      const { saved } = await appearance.uploadImage(picked);
      if (!saved) setMessage({ key: 'notSaved', tone: 'info' });
    } catch (reason) {
      setMessage({ key: reason instanceof ImageError ? PROBLEMS[reason.problem] : 'errorDecode', tone: 'error' });
    } finally {
      setBusy(false);
      if (file.current) file.current.value = '';
    }
  };
  return <>
    <button type="button" className="icon-button" aria-label={t('open')} title={t('open')} onClick={() => setOpen(true)}><Palette size={18} strokeWidth={1.9} aria-hidden="true" /></button>
    <Drawer open={open} variant="sheet" className="appearance-sheet" title={t('sheetTitle')} closeLabel={translate('ui.closeDialog', language)} onClose={() => setOpen(false)}>
      <p className="muted small">{t('sheetHelp')}</p>
      <fieldset className="appearance-section">
        <legend className="group-title">{t('solidGroup')}</legend>
        <div className="bg-options">{SOLID_BACKGROUNDS.map(option)}</div>
      </fieldset>
      <fieldset className="appearance-section">
        <legend className="group-title">{t('patternGroup')}</legend>
        <div className="bg-options">{PATTERN_BACKGROUNDS.map(option)}</div>
      </fieldset>
      <fieldset className="appearance-section">
        <legend className="group-title">{t('customGroup')}</legend>
        {image && <div className="bg-options"><Swatch id="custom" name={name} checked={background === 'custom'} label={t('bg_custom')} image={image.url} onSelect={() => appearance.changeBackground('custom')} /></div>}
        <p className="muted small">{t('uploadHelp')}</p>
        <input ref={file} type="file" className="sr-only" tabIndex={-1} aria-hidden="true" accept={ACCEPTED_TYPES.join(',')} onChange={(event) => void upload(event.target.files?.[0])} />
        <div className="button-group">
          <button type="button" className="button secondary" disabled={busy} onClick={() => file.current?.click()}><ImagePlus size={16} aria-hidden="true" />{busy ? t('preparing') : t(image ? 'replace' : 'choose')}</button>
          {image && <button type="button" className="button quiet" disabled={busy} onClick={() => { setMessage(null); void appearance.removeImage(); }}><Trash2 size={16} aria-hidden="true" />{t('remove')}</button>}
        </div>
        {message && <p className={message.tone === 'error' ? 'notice error' : 'notice'} role={message.tone === 'error' ? 'alert' : 'status'}>{t(message.key)}</p>}
        {image && background === 'custom' && <label className="overlay-control">
          <span className="overlay-label"><span>{t('overlay')}</span><output className="num">{t('overlayValue', { value: overlay })}</output></span>
          <input type="range" min={0} max={MAX_OVERLAY} step={5} value={overlay} onChange={(event) => appearance.changeOverlay(Number(event.target.value))} />
          <small className="muted">{t('overlayHelp')}</small>
        </label>}
      </fieldset>
    </Drawer>
  </>;
}
