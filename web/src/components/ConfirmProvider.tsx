import { createContext, useCallback, useContext, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react';
import { translate, type Language } from '../i18n';
import { Drawer } from './beui';

type ConfirmAction = (messageKey: string, actionKey: string, returnFocus?: HTMLElement) => Promise<boolean>;
const ConfirmContext = createContext<ConfirmAction | null>(null);

export function ConfirmProvider({ language, children }: { language: Language; children: ReactNode }) {
  const [request, setRequest] = useState<{ messageKey: string; actionKey: string } | null>(null);
  const pending = useRef<((accepted: boolean) => void) | null>(null);
  const focusAfter = useRef<HTMLElement | null>(null);
  useEffect(() => () => { pending.current?.(false); pending.current = null; }, []);
  useLayoutEffect(() => {
    if (request || !focusAfter.current) return;
    const target = focusAfter.current;
    focusAfter.current = null;
    if (target.isConnected && !target.hasAttribute('disabled')) target.focus();
  }, [request]);

  const settle = useCallback((accepted: boolean) => {
    const resolve = pending.current;
    pending.current = null;
    setRequest(null);
    resolve?.(accepted);
  }, []);
  const ask = useCallback<ConfirmAction>((messageKey, actionKey, returnFocus) => {
    if (pending.current) return Promise.resolve(false);
    focusAfter.current = returnFocus ?? (document.activeElement instanceof HTMLElement ? document.activeElement : null);
    return new Promise<boolean>((resolve) => {
      pending.current = resolve;
      setRequest({ messageKey, actionKey });
    });
  }, []);

  return <ConfirmContext.Provider value={ask}>
    {children}
    <Drawer open={request !== null} title={translate('ui.confirm', language)} closeLabel={translate('ui.closeDialog', language)} onClose={() => settle(false)}>
      {request && <><p>{translate(request.messageKey, language)}</p>
        <div className="button-group confirm-actions">
          <button type="button" className="button secondary" onClick={() => settle(false)}>{translate('ui.cancel', language)}</button>
          <button type="button" className="button primary" onClick={() => settle(true)}>{translate(request.actionKey, language)}</button>
        </div></>}
    </Drawer>
  </ConfirmContext.Provider>;
}

export function useConfirm(): ConfirmAction {
  const confirmAction = useContext(ConfirmContext);
  if (!confirmAction) throw new Error('Confirmation provider is missing');
  return confirmAction;
}
