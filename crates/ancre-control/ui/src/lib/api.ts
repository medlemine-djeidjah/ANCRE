/**
 * The control plane, typed.
 *
 * Everything goes through one `request` so that a 401 has exactly one meaning
 * in one place: the session is gone, show the login screen. A fetch scattered
 * across components ends up with five different opinions about that.
 */

export class Unauthenticated extends Error {
  constructor() {
    super("not authenticated");
    this.name = "Unauthenticated";
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    ...init,
    // The session is an HttpOnly cookie, so it has to be sent explicitly and
    // cannot be read by this code — which is the point of it being HttpOnly.
    credentials: "same-origin",
    headers: { "content-type": "application/json", ...(init?.headers ?? {}) },
  });

  if (res.status === 401) throw new Unauthenticated();
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(body || `${res.status} ${res.statusText}`);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

// --- session ---------------------------------------------------------------

export const session = {
  status: () => request<{ authenticated: boolean }>("/api/session"),
  login: (token: string) =>
    request<void>("/api/session", {
      method: "POST",
      body: JSON.stringify({ token }),
    }),
  logout: () => request<void>("/api/session", { method: "DELETE" }),
};

// --- shapes, mirroring the Rust ---------------------------------------------

export interface ChainListing {
  tenant_id: string;
  system_id: string;
  head_seq: number;
  event_count: number;
}

export interface FlagCount {
  name: string;
  count: number;
}

export interface ChainSummary {
  tenant_id: string;
  system_id: string;
  head_seq: number;
  event_count: number;
  first_event_at: number | null;
  last_event_at: number | null;
  risk_flags: FlagCount[];
  event_types: FlagCount[];
  outcomes: FlagCount[];
  generations: number;
  model_versions: FlagCount[];
  events_with_gaps: number;
}

export interface Pins {
  config_generation: number;
  config_hash: string;
  system_id: string;
  system_version: string;
  ifu_version: string;
  model_id: string;
  model_version: string;
  prompt_id: string;
  prompt_version: string;
  policy_id: string;
  policy_version: string;
  gateway_version: string;
  risk_class: string;
  resolved_stale: boolean;
  risk_flags: string[];
}

export interface Metrics {
  provider: string;
  http_status: number;
  latency_ms: number;
  ttft_ms: number;
  tokens_in: number;
  tokens_out: number;
  error_code: string;
}

export interface AuditEvent {
  seq: number;
  prev_hash: string;
  event_hash: string;
  canon_version: string;
  ingested_at: number;
  emitted: {
    tenant_id: string;
    system_id: string;
    event_id: string;
    trace_id: string;
    attempt_seq: number;
    occurred_at: number;
    node_id: string;
    event_type: string;
    outcome: string;
    pins: Pins;
    request_digest: string;
    response_digest: string;
    metrics: Metrics;
  };
}

export interface CheckpointBody {
  tenant_id: string;
  system_id: string;
  seq_from: number;
  seq_to: number;
  root_hash: string;
  built_at: number;
  canon_version: string;
}

export interface Checkpoint {
  body: CheckpointBody;
  signature: string;
  key_id: string;
}

export interface PublicKeyRecord {
  key_id: string;
  public_key: string;
  valid_from: number;
  valid_to: number | null;
}

// --- reads ------------------------------------------------------------------

export const api = {
  chains: () => request<ChainListing[]>("/v1/chains"),

  summary: (tenant: string, system: string) =>
    request<ChainSummary>(
      `/v1/chains/${encodeURIComponent(tenant)}/${encodeURIComponent(system)}/summary`,
    ),

  checkpoints: (tenant: string, system: string) =>
    request<Checkpoint[]>(
      `/v1/checkpoints/${encodeURIComponent(tenant)}/${encodeURIComponent(system)}`,
    ),

  pubkeys: () => request<PublicKeyRecord[]>("/v1/pubkeys"),

  /**
   * A page of the chain. The endpoint streams newline-delimited JSON rather
   * than an array, because it is the same bytes `ancre-verify --chain -`
   * reads — so this parses lines rather than calling `res.json()`.
   */
  async events(
    tenant: string,
    system: string,
    from: number,
    to: number,
  ): Promise<AuditEvent[]> {
    const res = await fetch(
      `/v1/chains/${encodeURIComponent(tenant)}/${encodeURIComponent(system)}/events?from=${from}&to=${to}`,
      { credentials: "same-origin" },
    );
    if (res.status === 401) throw new Unauthenticated();
    if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);

    const text = await res.text();
    return text
      .split("\n")
      .filter((line) => line.trim().length > 0)
      .map((line) => JSON.parse(line) as AuditEvent);
  },
};

/**
 * The four files of an evidence pack, assembled in the browser and downloaded
 * as one folder's worth of blobs.
 *
 * Deliberately client-side. The server could zip these, but then the artefact
 * an auditor verifies would have passed through one more piece of server code
 * that nobody has reason to trust — and the whole point of the pack is that it
 * is checkable without trusting the thing that produced it. Assembling it here
 * from the same three endpoints `quickstart.sh` curls keeps the provenance
 * boringly obvious.
 */
export async function buildPack(tenant: string, system: string, head: number) {
  const [events, checkpoints, pubkeys] = await Promise.all([
    api.events(tenant, system, 1, head),
    api.checkpoints(tenant, system),
    api.pubkeys(),
  ]);

  const manifest = {
    pack_version: "ancre-pack/1",
    tenant_id: tenant,
    system_id: system,
    created_at: new Date().toISOString(),
    produced_by: "ancre dashboard",
    source: window.location.origin,
    seq_from: events.length > 0 ? events[0].seq : null,
    seq_to: events.length > 0 ? events[events.length - 1].seq : null,
  };

  return {
    "manifest.json": JSON.stringify(manifest, null, 2),
    "events.jsonl": events.map((e) => JSON.stringify(e)).join("\n") + "\n",
    "checkpoints.json": JSON.stringify(checkpoints),
    "pubkeys.json": JSON.stringify(pubkeys),
  } satisfies Record<string, string>;
}
