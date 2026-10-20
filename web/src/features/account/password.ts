/** Rust limits passwords to 12–256 UTF-8 bytes; the console checks the same bytes before sending. */
export function passwordProblem(password: string): 'account.passwordShort' | 'account.passwordLong' | null {
  const bytes = new TextEncoder().encode(password).length;
  return bytes < 12 ? 'account.passwordShort' : bytes > 256 ? 'account.passwordLong' : null;
}
