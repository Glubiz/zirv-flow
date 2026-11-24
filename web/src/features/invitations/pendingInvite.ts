/** Where an invitation waits while the browser leaves for single sign-on, which drops the return path. */
export const PENDING_INVITE_KEY = "zf-invite";

function store(): Storage | null {
  try {
    return globalThis.sessionStorage;
  } catch {
    return null;
  }
}

export function rememberInvite(token: string): void {
  store()?.setItem(PENDING_INVITE_KEY, token);
}

export function clearPendingInvite(): void {
  store()?.removeItem(PENDING_INVITE_KEY);
}

/** The token that is waiting, left in place: the invitation page forgets it once it opens. */
export function peekPendingInvite(): string | null {
  return store()?.getItem(PENDING_INVITE_KEY) ?? null;
}
