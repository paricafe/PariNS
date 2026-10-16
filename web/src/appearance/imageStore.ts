// The custom background image, kept only in this browser's IndexedDB (never uploaded).
// Failures resolve to "nothing stored" / false so the console works without IndexedDB.

const DATABASE = 'parins-appearance';
const STORE = 'background';
const KEY = 'image';

export interface StoredImage { blob: Blob; luminance: number }

function open(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DATABASE, 1);
    request.onupgradeneeded = () => { request.result.createObjectStore(STORE); };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
    request.onblocked = () => reject(new Error('blocked'));
  });
}

async function run<T>(mode: IDBTransactionMode, action: (store: IDBObjectStore) => IDBRequest): Promise<T> {
  const database = await open();
  try {
    return await new Promise<T>((resolve, reject) => {
      const transaction = database.transaction(STORE, mode);
      const request = action(transaction.objectStore(STORE));
      transaction.oncomplete = () => resolve(request.result as T);
      transaction.onerror = () => reject(transaction.error);
      transaction.onabort = () => reject(transaction.error);
    });
  } finally { database.close(); }
}

export async function loadImage(): Promise<StoredImage | null> {
  try {
    const value = await run<unknown>('readonly', (store) => store.get(KEY));
    const record = value as Partial<StoredImage> | undefined;
    return record?.blob instanceof Blob && typeof record.luminance === 'number' ? { blob: record.blob, luminance: record.luminance } : null;
  } catch { return null; }
}

export async function saveImage(image: StoredImage): Promise<boolean> {
  try { await run('readwrite', (store) => store.put(image, KEY)); return true; } catch { return false; }
}

export async function clearImage(): Promise<boolean> {
  try { await run('readwrite', (store) => store.delete(KEY)); return true; } catch { return false; }
}
