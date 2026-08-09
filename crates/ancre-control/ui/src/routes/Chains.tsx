import { useEffect, useState } from "react";
import { Link } from "react-router";
import { ChevronRight, Database } from "lucide-react";
import { api, type ChainListing } from "@/lib/api";
import { Card, CardHeader, Empty } from "@/components/ui";
import { thousands } from "@/lib/format";

export function Chains() {
  const [chains, setChains] = useState<ChainListing[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .chains()
      .then(setChains)
      .catch((e: Error) => setError(e.message));
  }, []);

  const total = chains?.reduce((n, c) => n + c.event_count, 0) ?? 0;

  return (
    <div className="mx-auto max-w-5xl px-6 py-10">
      <header className="mb-6">
        <h1 className="text-xl font-semibold tracking-tight">Chains</h1>
        <p className="mt-1 text-sm text-[var(--color-muted)]">
          One chain per{" "}
          <code className="text-[var(--color-ink)]">(tenant, system)</code>. A
          system with no traffic has no chain, which is why a quiet system
          emits a daily heartbeat — absence of evidence and absence of a system
          must not look the same.
        </p>
      </header>

      <Card>
        <CardHeader
          title={
            chains
              ? `${chains.length} chain${chains.length === 1 ? "" : "s"}`
              : "Loading…"
          }
          hint={
            chains && chains.length > 0
              ? `${thousands(total)} events recorded`
              : undefined
          }
        />

        {error ? (
          <Empty>
            <span className="text-[var(--color-bad)]">{error}</span>
          </Empty>
        ) : !chains ? (
          <Empty>Loading…</Empty>
        ) : chains.length === 0 ? (
          <Empty>
            No chains yet. Send a request through the gateway and it will
            appear here once the ingester has chained it.
          </Empty>
        ) : (
          <ul>
            {chains.map((c) => (
              <li
                key={`${c.tenant_id}/${c.system_id}`}
                className="border-b border-[var(--color-line-soft)] last:border-0"
              >
                <Link
                  to={`/chains/${encodeURIComponent(c.tenant_id)}/${encodeURIComponent(c.system_id)}`}
                  className="flex items-center gap-4 px-5 py-4 hover:bg-[var(--color-raised)]"
                >
                  <Database className="size-4 shrink-0 text-[var(--color-faint)]" />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-sm font-medium">
                      {c.system_id}
                    </div>
                    <div className="mt-0.5 text-xs text-[var(--color-muted)]">
                      {c.tenant_id}
                    </div>
                  </div>
                  <div className="tnum shrink-0 text-right">
                    <div className="text-sm font-medium">
                      {thousands(c.event_count)}
                    </div>
                    <div className="text-xs text-[var(--color-muted)]">
                      seq 1–{thousands(c.head_seq)}
                    </div>
                  </div>
                  <ChevronRight className="size-4 shrink-0 text-[var(--color-faint)]" />
                </Link>
              </li>
            ))}
          </ul>
        )}
      </Card>
    </div>
  );
}
