// HTTP client: same-origin fetch with the session cookie and CSRF header.
// The UI never parses natural language — all server responses are typed
// envelopes with error codes.

export class ApiError extends Error {
  code: string;
  details: string[];
  constructor(code: string, message: string, details: string[]) {
    super(message);
    this.code = code;
    this.details = details;
  }
}

let csrfToken = "";

export function setCsrf(token: string) {
  csrfToken = token;
}

async function handle(resp: Response): Promise<any> {
  if (resp.status === 204) return {};
  let body: any = null;
  try {
    body = await resp.json();
  } catch {
    body = null;
  }
  if (!resp.ok) {
    const err = body?.error ?? {};
    throw new ApiError(err.code ?? "E_INTERNAL", err.message ?? `HTTP ${resp.status}`, err.details ?? []);
  }
  return body;
}

function headers(csrf: boolean, idempotency?: string, ifMatch?: number): Record<string, string> {
  const h: Record<string, string> = {};
  if (csrf) h["X-RelayCheck-CSRF"] = csrfToken;
  if (idempotency) h["Idempotency-Key"] = idempotency;
  if (ifMatch !== undefined) h["If-Match"] = String(ifMatch);
  return h;
}

export const api = {
  async exchange(token: string): Promise<{ csrf: string }> {
    const r = await fetch("/v1/session/exchange", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token }),
    });
    return handle(r);
  },
  async me(): Promise<{ csrf: string; version: string } | null> {
    try {
      return await handle(await fetch("/v1/me"));
    } catch {
      return null;
    }
  },
  async mintBootstrap(): Promise<{ url_fragment: string }> {
    return handle(await fetch("/v1/session/bootstrap", { method: "POST", headers: headers(true) }));
  },
  async state(): Promise<any> {
    return handle(await fetch("/v1/state"));
  },
  async task(id: string): Promise<any> {
    return handle(await fetch(`/v1/tasks/${encodeURIComponent(id)}`));
  },
  async createTask(body: any, idempotency: string): Promise<any> {
    return handle(
      await fetch("/v1/tasks", {
        method: "POST",
        headers: { ...headers(true, idempotency), "Content-Type": "application/json" },
        body: JSON.stringify(body),
      }),
    );
  },
  async capturePlan(taskId: string, body: any): Promise<any> {
    return handle(
      await fetch(`/v1/tasks/${encodeURIComponent(taskId)}/capture/plan`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      }),
    );
  },
  async capture(taskId: string, body: any, idempotency: string, ifMatch: number): Promise<any> {
    return handle(
      await fetch(`/v1/tasks/${encodeURIComponent(taskId)}/capture`, {
        method: "POST",
        headers: { ...headers(true, idempotency, ifMatch), "Content-Type": "application/json" },
        body: JSON.stringify(body),
      }),
    );
  },
  async saveProfile(taskId: string, body: any, ifMatch: number): Promise<any> {
    return handle(
      await fetch(`/v1/tasks/${encodeURIComponent(taskId)}/profile`, {
        method: "POST",
        headers: { ...headers(true, undefined, ifMatch), "Content-Type": "application/json" },
        body: JSON.stringify(body),
      }),
    );
  },
  async verify(taskId: string, identity: { profile_id: string; profile_hash: string }, ifMatch: number, idempotency: string): Promise<any> {
    return handle(
      await fetch(`/v1/tasks/${encodeURIComponent(taskId)}/verify`, {
        method: "POST",
        headers: { ...headers(true, idempotency, ifMatch), "Content-Type": "application/json" },
        body: JSON.stringify({ trust_confirmed: true, ...identity }),
      }),
    );
  },
  async operation(id: string): Promise<any> {
    return handle(await fetch(`/v1/operations/${encodeURIComponent(id)}`));
  },
  async cancelOperation(id: string): Promise<any> {
    return handle(
      await fetch(`/v1/operations/${encodeURIComponent(id)}/cancel`, {
        method: "POST",
        headers: headers(true),
      }),
    );
  },
  async createHandoff(taskId: string, body: any, idempotency: string, ifMatch: number): Promise<any> {
    return handle(
      await fetch(`/v1/tasks/${encodeURIComponent(taskId)}/handoffs`, {
        method: "POST",
        headers: { ...headers(true, idempotency, ifMatch), "Content-Type": "application/json" },
        body: JSON.stringify(body),
      }),
    );
  },
  handoffPackageUrl(handoffId: string): string {
    return `/v1/handoffs/${encodeURIComponent(handoffId)}/package`;
  },
  async importPackage(taskId: string, file: File, ifMatch: number): Promise<any> {
    return handle(
      await fetch(`/v1/tasks/${encodeURIComponent(taskId)}/imports`, {
        method: "POST",
        headers: headers(true, undefined, ifMatch),
        body: file,
      }),
    );
  },
  async confirmCandidate(taskId: string, snapshotId: string, ifMatch: number): Promise<any> {
    return handle(
      await fetch(`/v1/tasks/${encodeURIComponent(taskId)}/candidate`, {
        method: "POST",
        headers: { ...headers(true, undefined, ifMatch), "Content-Type": "application/json" },
        body: JSON.stringify({ snapshot_id: snapshotId }),
      }),
    );
  },
  async accept(taskId: string, criteriaAck: string[], ifMatch: number, idempotency: string): Promise<any> {
    return handle(
      await fetch(`/v1/tasks/${encodeURIComponent(taskId)}/acceptances`, {
        method: "POST",
        headers: { ...headers(true, idempotency, ifMatch), "Content-Type": "application/json" },
        body: JSON.stringify({ criteria_ack: criteriaAck }),
      }),
    );
  },
  async waitOperation(opId: string): Promise<any> {
    for (;;) {
      const op = await this.operation(opId);
      const st = op.state as string;
      if (st === "completed" || st === "failed" || st === "cancelled" || st === "interrupted") return op;
      await new Promise((res) => setTimeout(res, 500));
    }
  },
  async claim(code: string): Promise<any> {
    return handle(
      await fetch("/v1/claims", {
        method: "POST",
        headers: { ...headers(true), "Content-Type": "application/json" },
        body: JSON.stringify({ code }),
      }),
    );
  },
  async demo(): Promise<any> {
    return handle(await fetch("/v1/demo", { method: "POST", headers: headers(true) }));
  },
  eventsUrl(after: number): string {
    return `/v1/events?after=${after}`;
  },
};
