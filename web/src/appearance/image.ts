// In-browser preparation of a custom background: decode, downscale to the long-edge limit,
// re-encode under the byte budget and measure mean luminance for the text-backing scrim.

export const ACCEPTED_TYPES = ['image/png', 'image/jpeg', 'image/webp'] as const;
export const MAX_EDGE = 2560;
export const MAX_BYTES = 1.5 * 1024 * 1024;
/** Bounds decode memory: a larger source file is refused before it is decoded. */
export const MAX_INPUT_BYTES = 40 * 1024 * 1024;
/** A highly compressed file can still expand to gigabytes when decoded at full size. */
export const MAX_INPUT_PIXELS = 100_000_000;
const QUALITIES = [0.86, 0.78, 0.7, 0.6];

export type ImageProblem = 'type' | 'size' | 'decode' | 'encode';
export class ImageError extends Error {
  constructor(readonly problem: ImageProblem) { super(problem); }
}

export interface PreparedImage { blob: Blob; width: number; height: number; luminance: number }

export function fitWithin(width: number, height: number, edge = MAX_EDGE): { width: number; height: number } {
  const scale = Math.min(1, edge / Math.max(width, height));
  return { width: Math.max(1, Math.round(width * scale)), height: Math.max(1, Math.round(height * scale)) };
}

function encode(canvas: HTMLCanvasElement, type: string, quality: number): Promise<Blob | null> {
  return new Promise((resolve) => canvas.toBlob(resolve, type, quality));
}

function draw(source: CanvasImageSource, width: number, height: number): HTMLCanvasElement {
  const canvas = document.createElement('canvas');
  canvas.width = width; canvas.height = height;
  const context = canvas.getContext('2d');
  if (!context) throw new ImageError('encode');
  context.imageSmoothingQuality = 'high';
  context.drawImage(source, 0, 0, width, height);
  return canvas;
}

/** Mean relative luminance (0–1) of an 8×8 thumbnail; 0.5 when the pixels cannot be read. */
function luminance(source: CanvasImageSource): number {
  try {
    const data = draw(source, 8, 8).getContext('2d')!.getImageData(0, 0, 8, 8).data;
    const linear = (value: number) => { const c = value / 255; return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4; };
    let total = 0;
    for (let index = 0; index < data.length; index += 4) total += 0.2126 * linear(data[index]) + 0.7152 * linear(data[index + 1]) + 0.0722 * linear(data[index + 2]);
    return total / (data.length / 4);
  } catch { return 0.5; }
}

/** Intrinsic size from the PNG, JPEG or WebP header without decoding; null when unrecognised. */
export function headerSize(bytes: Uint8Array): { width: number; height: number } | null {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const ascii = (at: number, text: string) => [...text].every((c, i) => bytes[at + i] === c.charCodeAt(0));
  if (bytes.length >= 24 && bytes[0] === 0x89 && ascii(1, 'PNG') && ascii(12, 'IHDR')) {
    return { width: view.getUint32(16), height: view.getUint32(20) };
  }
  if (bytes.length >= 30 && ascii(0, 'RIFF') && ascii(8, 'WEBP')) {
    if (ascii(12, 'VP8X')) {
      const u24 = (at: number) => bytes[at] | (bytes[at + 1] << 8) | (bytes[at + 2] << 16);
      return { width: u24(24) + 1, height: u24(27) + 1 };
    }
    if (ascii(12, 'VP8L') && bytes[20] === 0x2f) {
      const bits = view.getUint32(21, true);
      return { width: (bits & 0x3fff) + 1, height: ((bits >> 14) & 0x3fff) + 1 };
    }
    if (ascii(12, 'VP8 ')) return { width: view.getUint16(26, true) & 0x3fff, height: view.getUint16(28, true) & 0x3fff };
    return null;
  }
  if (bytes[0] === 0xff && bytes[1] === 0xd8) {
    // Walk JPEG segments to the first start-of-frame marker.
    let at = 2;
    while (at + 9 < bytes.length && bytes[at] === 0xff) {
      const marker = bytes[at + 1];
      const length = view.getUint16(at + 2);
      if (marker >= 0xc0 && marker <= 0xcf && ![0xc4, 0xc8, 0xcc].includes(marker)) {
        return { width: view.getUint16(at + 7), height: view.getUint16(at + 5) };
      }
      at += 2 + length;
    }
  }
  return null;
}

export async function prepareImage(file: Blob): Promise<PreparedImage> {
  if (!(ACCEPTED_TYPES as readonly string[]).includes(file.type)) throw new ImageError('type');
  if (file.size > MAX_INPUT_BYTES) throw new ImageError('size');
  // Headers are near the start; a JPEG's frame header may follow large metadata.
  const size = headerSize(new Uint8Array(await file.slice(0, 256 * 1024).arrayBuffer()));
  if (size && size.width * size.height > MAX_INPUT_PIXELS) throw new ImageError('size');
  let bitmap: ImageBitmap;
  try {
    // Decode straight to the target size when it is known, so the full-size image never
    // has to exist in memory.
    const target = size && fitWithin(size.width, size.height);
    bitmap = target && (target.width < size.width || target.height < size.height)
      ? await createImageBitmap(file, { resizeWidth: target.width, resizeHeight: target.height, resizeQuality: 'high' })
      : await createImageBitmap(file);
  } catch { throw new ImageError('decode'); }
  try {
    let { width, height } = fitWithin(bitmap.width, bitmap.height);
    const measured = luminance(bitmap);
    // Shrink further only when no quality step fits the byte budget (bounded: at most five sizes).
    for (let attempt = 0; attempt < 5; attempt += 1) {
      const canvas = draw(bitmap, width, height);
      for (const quality of QUALITIES) {
        let blob = await encode(canvas, 'image/webp', quality);
        // Browsers without a WebP encoder return PNG; JPEG keeps the budget meaningful there.
        if (blob && blob.type !== 'image/webp') blob = await encode(canvas, 'image/jpeg', quality);
        if (blob && blob.size <= MAX_BYTES) return { blob, width, height, luminance: measured };
      }
      ({ width, height } = fitWithin(width, height, Math.round(Math.max(width, height) * 0.8)));
    }
    throw new ImageError('encode');
  } finally { bitmap.close?.(); }
}
