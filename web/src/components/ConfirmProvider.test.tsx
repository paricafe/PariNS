// @vitest-environment jsdom
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeAll, describe, expect, it } from 'vitest';
import { useState } from 'react';
import { ConfirmProvider, useConfirm } from './ConfirmProvider';

beforeAll(() => {
  HTMLDialogElement.prototype.showModal = function showModal() { this.setAttribute('open', ''); };
  HTMLDialogElement.prototype.close = function close() { this.removeAttribute('open'); };
});

function Probe() {
  const confirmAction = useConfirm();
  const [result, setResult] = useState('pending');
  return <><button type="button" onClick={(event) => { event.currentTarget.blur(); void confirmAction('app.discardHelp', 'app.discard', event.currentTarget).then((accepted) => setResult(accepted ? 'accepted' : 'cancelled')); }}>Ask</button><span>{result}</span></>;
}

describe('console confirmation', () => {
  it('uses a translated modal and resolves cancel and confirm without a browser dialog', async () => {
    const view = render(<ConfirmProvider language="en"><Probe /></ConfirmProvider>);
    const trigger = screen.getByRole('button', { name: 'Ask' });
    trigger.focus();
    fireEvent.click(trigger);
    expect(screen.getByRole('dialog', { name: 'Confirm' })).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.getByText('cancelled')).toBeTruthy());
    expect(document.activeElement).toBe(trigger);

    fireEvent.click(trigger);
    view.rerender(<ConfirmProvider language="zh-CN"><Probe /></ConfirmProvider>);
    expect(screen.getByRole('dialog', { name: '确认' })).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: '放弃草稿并加载' }));
    await waitFor(() => expect(screen.getByText('accepted')).toBeTruthy());
  });
});
