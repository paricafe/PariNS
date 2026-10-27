/** Rust limits passwords to 12–256 UTF-8 bytes; the console checks the same bytes before sending. */
export function passwordProblem(password: string): 'account.passwordShort' | 'account.passwordLong' | null {
  const bytes = new TextEncoder().encode(password).length;
  return bytes < 12 ? 'account.passwordShort' : bytes > 256 ? 'account.passwordLong' : null;
}

/** Rust accepts 1–64 ASCII letters, digits, hyphens and underscores. */
export const validUsername = (username: string) => /^[A-Za-z0-9_-]{1,64}$/.test(username);
