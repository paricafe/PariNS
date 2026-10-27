import { useId, useState, type ReactNode } from 'react';
import { ChevronDown } from 'lucide-react';

/** Collapsed-by-default section with an animated height (CSS grid rows). */
export function Disclosure({ title, children, defaultOpen = false }: { title: string; children: ReactNode; defaultOpen?: boolean }) {
  const [open, setOpen] = useState(defaultOpen);
  const id = useId();
  return <section className="panel disclosure">
    <button type="button" className="disclosure-toggle" aria-expanded={open} aria-controls={id} onClick={() => setOpen((value) => !value)}>
      <span><strong>{title}</strong></span>
      <ChevronDown size={18} aria-hidden="true" className="disclosure-chevron" />
    </button>
    <div id={id} className="settings-expand" data-open={open} inert={!open}><div><div className="disclosure-body">{children}</div></div></div>
  </section>;
}
