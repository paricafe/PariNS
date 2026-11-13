import { useReducedMotion } from 'motion/react';

const DIGITS = '0123456789';

export function Odometer({ text }: { text: string }) {
  const reduced = useReducedMotion();
  const chars = [...text];
  return <span className="odometer">
    <span className="odometer-visual" aria-hidden="true">{chars.map((char, index) => {
      const key = chars.length - index;
      const digit = DIGITS.indexOf(char);
      return digit < 0 ? <span key={`c${key}`} className="odometer-char" data-char={char} />
        : <span key={`d${key}`} className="odometer-digit"><span className="odometer-reel" data-reduced={reduced || undefined} style={{ transform: `translateY(${-digit * 10}%)` }} /></span>;
    })}</span>
    <span className="sr-only">{text}</span>
  </span>;
}
