export function passwordProblem(password: string): 'account.passwordShort' | 'account.passwordLong' | null {
  const bytes = new TextEncoder().encode(password).length;
  return bytes < 12 ? 'account.passwordShort' : bytes > 256 ? 'account.passwordLong' : null;
}

export const validUsername = (username: string) => /^[A-Za-z0-9_-]{1,64}$/.test(username);
