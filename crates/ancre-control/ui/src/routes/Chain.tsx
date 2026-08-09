import { useCallback, useEffect, useState } from "react";
import { Link, useParams } from "react-router";
import { ArrowLeft, Download, ShieldQuestion, Terminal } from "lucide-react";
import {
  api,
  buildPack,
  type AuditEvent,
  type Checkpoint,
  type ChainSummary,
} from "@/lib/api";
import {
  Badge,
  Button,
  Card,
  CardHeader,
  Empty,
  Field,
  Hash,
  RiskClass,
  RiskFlag,
  Stat,
} from "@/components/ui";
import {
  cn,
  downloadFiles,
  formatInstant,
  formatSpan,
  formatTime,
  GAP_PINS,
  isGap,
  thousands,
} from "@/lib/format";

/** How many events one screenful loads. Matches the export endpoint's page. */
const PAGE = 200;

export function Chain() {
  const params = useParams<{ tenant: string; system: string }>();
  const tenant = params.tenant ?? "";
  const system = params.system ?? "";

  const [summary, setSummary] = useState<ChainSummary | null>(null);
  const [checkpoints, setCheckpoints] = useState<Checkpoint[]>([]);
  const [events, setEvents] = useState<AuditEvent[]>([]);
  const [selected, setSelected] = useState<AuditEvent | null>(null);
  const [flagOnly, setFlagOnly] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [packing, setPacking] = useState(false);

  useEffect(() => {
    let live = true;
    setSummary(null);
    setEvents([]);
    setSelected(null);

    void (async () => {
      try {
        const s = await api.summary(tenant, system);
        if (!live) return;
        setSummary(s);

        const [cps, evs] = await Promise.all([
          api.checkpoints(tenant, system),
          s.head_seq > 0
            ? api.events(tenant, system, Math.max(1, s.head_seq - PAGE + 1), s.head_seq)
            : Promise.resolve([]),
        ]);
        if (!live) return;
        setCheckpoints(cps);
        // Newest first: the question a dashboard is opened with is almost
        // always "what happened just now", not "what happened first".
        setEvents(evs.reverse());
      } catch (e) {
        if (live) setError((e as Error).message);
      }
    })();

    return () => {
      live = false;
    };
  }, [tenant, system]);

  const exportPack = useCallback(async () => {
    if (!summary) return;
    setPacking(true);
    try {
      const files = await buildPack(tenant, system, summary.head_seq);
      downloadFiles(`${tenant}-${system}`, files);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setPacking(false);
    }
  }, [summary, tenant, system]);

  if (error) {
    return (
      <div className="mx-auto max-w-6xl px-6 py-10">
        <Card>
          <Empty>
            <span className="text-[var(--color-bad)]">{error}</span>
          </Empty>
        </Card>
      </div>
    );
  }

  if (!summary) {
    return (
      <div className="mx-auto max-w-6xl px-6 py-10">
        <Card>
          <Empty>Loading…</Empty>
        </Card>
      </div>
    );
  }

  const attested = coveredRuns(checkpoints);
  const shown = flagOnly
    ? events.filter((e) => e.emitted.pins.risk_flags.length > 0)
    : events;

  return (
    <div className="mx-auto max-w-6xl px-6 py-10">
      <Link
        to="/"
        className="mb-5 inline-flex items-center gap-1.5 text-xs text-[var(--color-muted)] hover:text-[var(--color-ink)]"
      >
        <ArrowLeft className="size-3.5" />
        All chains
      </Link>

      <header className="mb-6 flex flex-wrap items-end justify-between gap-4">
        <div>
          <h1 className="text-xl font-semibold tracking-tight">{system}</h1>
          <p className="mt-1 text-sm text-[var(--color-muted)]">{tenant}</p>
        </div>
        <Button variant="primary" size="md" onClick={exportPack} disabled={packing}>
          <Download className="size-4" />
          {packing ? "Building…" : "Download evidence pack"}
        </Button>
      </header>

      <div className="grid gap-5 lg:grid-cols-3">
        <div className="space-y-5 lg:col-span-2">
          <Card>
            <div className="grid grid-cols-2 divide-x divide-[var(--color-line-soft)] sm:grid-cols-4">
              <Stat label="Events" value={thousands(summary.event_count)} />
              <Stat
                label="Sequence"
                value={summary.head_seq > 0 ? `1–${thousands(summary.head_seq)}` : "—"}
              />
              <Stat
                label="Configurations"
                value={summary.generations}
                hint="distinct generations"
              />
              <Stat
                label="Gaps"
                value={thousands(summary.events_with_gaps)}
                tone={summary.events_with_gaps > 0 ? "warn" : "ok"}
                hint="events with an unknown pin"
              />
            </div>
          </Card>

          <VerificationCard
            checkpoints={checkpoints}
            attested={attested}
            head={summary.head_seq}
            onExport={exportPack}
            packing={packing}
            slug={`${tenant}-${system}`}
          />

          <Card>
            <CardHeader
              title="Events"
              hint={
                events.length < summary.event_count
                  ? `Most recent ${thousands(events.length)} of ${thousands(summary.event_count)}`
                  : undefined
              }
              action={
                <Button
                  variant={flagOnly ? "primary" : "secondary"}
                  onClick={() => setFlagOnly((v) => !v)}
                >
                  Flagged only
                </Button>
              }
            />
            {shown.length === 0 ? (
              <Empty>
                {flagOnly
                  ? "No flagged events in this range."
                  : "No events yet."}
              </Empty>
            ) : (
              <EventTable
                events={shown}
                selected={selected}
                onSelect={setSelected}
              />
            )}
          </Card>
        </div>

        <div className="space-y-5">
          {selected ? (
            <EventDetail event={selected} onClose={() => setSelected(null)} />
          ) : (
            <Card>
              <CardHeader
                title="Shape of this chain"
                hint="Counted by the control plane, not verified by it."
              />
              <div className="space-y-4 px-5 py-4">
                <Counts title="Risk flags" rows={summary.risk_flags} warn />
                <Counts title="Model versions" rows={summary.model_versions} />
                <Counts title="Event types" rows={summary.event_types} />
                <Counts title="Outcomes" rows={summary.outcomes} />
                {summary.first_event_at && summary.last_event_at ? (
                  <div className="border-t border-[var(--color-line-soft)] pt-3">
                    <Field label="First event">
                      {formatInstant(summary.first_event_at)}
                    </Field>
                    <Field label="Last event">
                      {formatInstant(summary.last_event_at)}
                    </Field>
                    <Field label="Span">
                      {formatSpan(summary.first_event_at, summary.last_event_at)}
                    </Field>
                  </div>
                ) : null}
              </div>
            </Card>
          )}
        </div>
      </div>
    </div>
  );
}

/**
 * The card that refuses to give you a green tick.
 *
 * Every instinct of dashboard design says to put a large ✓ VERIFIED here. It
 * would be a lie of exactly the kind this product exists to prevent: the
 * server rendering this page is the server that serves the events, and a claim
 * it makes about its own records is worth nothing to an auditor. So this
 * states what is actually known — which ranges carry a signature — and hands
 * over the artefact that can be checked somewhere else.
 */
function VerificationCard({
  checkpoints,
  attested,
  head,
  onExport,
  packing,
  slug,
}: {
  checkpoints: Checkpoint[];
  attested: Array<[number, number]>;
  head: number;
  onExport: () => void;
  packing: boolean;
  slug: string;
}) {
  const fullyCovered =
    attested.length === 1 && attested[0][0] <= 1 && attested[0][1] >= head;

  return (
    <Card>
      <CardHeader
        title={
          <span className="flex items-center gap-2">
            <ShieldQuestion className="size-4 text-[var(--color-accent)]" />
            Verification happens on your machine, not here
          </span>
        }
        hint="This page is the same server that stores the events. Anything it says about their integrity is unverifiable by definition — so it does not say it."
      />

      <div className="space-y-4 px-5 py-4">
        {checkpoints.length === 0 ? (
          <p className="text-sm text-[var(--color-warn)]">
            Nothing has signed this chain yet. Expected for a chain younger than
            one checkpoint interval; a finding for one that is not.
          </p>
        ) : (
          <div className="space-y-2">
            <div className="flex flex-wrap items-center gap-2 text-sm">
              <Badge tone={fullyCovered ? "ok" : "warn"}>
                {checkpoints.length} signed checkpoint
                {checkpoints.length === 1 ? "" : "s"}
              </Badge>
              <span className="text-[var(--color-muted)]">covering</span>
              <span className="tnum font-medium">
                {attested.map(([a, b]) => `seq ${a}–${b}`).join(", ")}
              </span>
              {!fullyCovered && head > 0 ? (
                <span className="text-xs text-[var(--color-warn)]">
                  of 1–{head}
                </span>
              ) : null}
            </div>
            <p className="text-xs leading-relaxed text-[var(--color-muted)]">
              A checkpoint is an ed25519 signature over a Merkle root of its
              range. Whether these signatures are <em>valid</em>, and whether
              the events still produce those roots, is what the verifier
              decides.
            </p>
          </div>
        )}

        <div className="rounded-lg border border-[var(--color-line)] bg-[var(--color-bg)] p-4">
          <div className="mb-2.5 flex items-center gap-1.5 text-xs font-medium text-[var(--color-muted)]">
            <Terminal className="size-3.5" />
            To actually verify it
          </div>
          <pre className="hash overflow-x-auto text-[var(--color-ink)]">
            <code>{`mkdir ${slug} && mv ${slug}-*.json* ${slug}/
ancre-verify --pack ./${slug}`}</code>
          </pre>
          <p className="mt-3 text-xs leading-relaxed text-[var(--color-muted)]">
            The verifier opens no sockets and reads only the files you name. Ask
            for the signing key fingerprint through some other channel than this
            one and pass it as{" "}
            <code className="text-[var(--color-ink)]">--key</code> — a pack
            checked against the keys inside it proves consistency, not
            provenance.
          </p>
          <Button className="mt-3" onClick={onExport} disabled={packing}>
            <Download className="size-3.5" />
            {packing ? "Building…" : "Download the pack"}
          </Button>
        </div>
      </div>
    </Card>
  );
}

function Counts({
  title,
  rows,
  warn,
}: {
  title: string;
  rows: Array<{ name: string; count: number }>;
  warn?: boolean;
}) {
  if (rows.length === 0) {
    return (
      <div>
        <div className="mb-1.5 text-xs font-medium uppercase tracking-wider text-[var(--color-faint)]">
          {title}
        </div>
        <div className="text-sm text-[var(--color-muted)]">None</div>
      </div>
    );
  }

  return (
    <div>
      <div className="mb-1.5 text-xs font-medium uppercase tracking-wider text-[var(--color-faint)]">
        {title}
      </div>
      <div className="space-y-1">
        {rows.slice(0, 6).map((r) => (
          <div key={r.name} className="flex items-baseline justify-between gap-3">
            <span
              className={cn(
                "min-w-0 truncate text-[13px]",
                warn ? "text-[var(--color-warn)]" : "text-[var(--color-ink)]",
              )}
              title={r.name}
            >
              {warn ? r.name.replace(/_/g, " ") : r.name}
            </span>
            <span className="tnum shrink-0 text-[13px] text-[var(--color-muted)]">
              {thousands(r.count)}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}

function EventTable({
  events,
  selected,
  onSelect,
}: {
  events: AuditEvent[];
  selected: AuditEvent | null;
  onSelect: (e: AuditEvent) => void;
}) {
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-left text-[13px]">
        <thead>
          <tr className="border-b border-[var(--color-line-soft)] text-xs text-[var(--color-faint)]">
            <th className="px-5 py-2.5 font-medium">seq</th>
            <th className="py-2.5 font-medium">time</th>
            <th className="py-2.5 font-medium">model version</th>
            <th className="py-2.5 font-medium">outcome</th>
            <th className="px-5 py-2.5 font-medium">flags</th>
          </tr>
        </thead>
        <tbody>
          {events.map((e) => {
            const pins = e.emitted.pins;
            const gap = isGap(pins.model_version);
            return (
              <tr
                key={e.seq}
                onClick={() => onSelect(e)}
                className={cn(
                  "cursor-pointer border-b border-[var(--color-line-soft)] last:border-0",
                  selected?.seq === e.seq
                    ? "bg-[var(--color-raised)]"
                    : "hover:bg-[var(--color-raised)]",
                )}
              >
                <td className="tnum px-5 py-2.5 text-[var(--color-muted)]">
                  {e.seq}
                </td>
                <td className="tnum py-2.5 text-[var(--color-muted)]">
                  {formatTime(e.emitted.occurred_at)}
                </td>
                <td
                  className={cn(
                    "hash max-w-[16rem] truncate py-2.5",
                    gap ? "text-[var(--color-warn)]" : "text-[var(--color-ink)]",
                  )}
                  title={pins.model_version}
                >
                  {pins.model_version}
                </td>
                <td className="py-2.5">
                  <Badge
                    tone={
                      e.emitted.outcome === "ok"
                        ? "neutral"
                        : e.emitted.outcome === "error"
                          ? "bad"
                          : "warn"
                    }
                  >
                    {e.emitted.outcome}
                  </Badge>
                </td>
                <td className="px-5 py-2.5">
                  <div className="flex flex-wrap gap-1">
                    {pins.risk_flags.map((f) => (
                      <RiskFlag key={f} name={f} />
                    ))}
                  </div>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function EventDetail({
  event,
  onClose,
}: {
  event: AuditEvent;
  onClose: () => void;
}) {
  const { emitted } = event;
  const pins = emitted.pins;

  return (
    <Card>
      <CardHeader
        title={`Event ${event.seq}`}
        hint={emitted.event_type}
        action={
          <Button variant="ghost" onClick={onClose}>
            Close
          </Button>
        }
      />
      <div className="px-5 py-4">
        <div className="mb-3 flex flex-wrap items-center gap-1.5">
          <RiskClass value={pins.risk_class} />
          {pins.resolved_stale ? <Badge tone="warn">stale config</Badge> : null}
          {pins.risk_flags.map((f) => (
            <RiskFlag key={f} name={f} />
          ))}
        </div>

        <Section title="Pins">
          <dl>
            {GAP_PINS.map((k) => (
              <Field
                key={k}
                label={k.replace(/_/g, " ")}
                tone={isGap(pins[k]) ? "warn" : undefined}
              >
                {pins[k]}
              </Field>
            ))}
            <Field label="config generation">{pins.config_generation}</Field>
            <Field label="config hash">
              <Hash value={pins.config_hash} />
            </Field>
          </dl>
        </Section>

        <Section title="Chain">
          <dl>
            <Field label="event hash">
              <Hash value={event.event_hash} />
            </Field>
            <Field label="prev hash">
              <Hash value={event.prev_hash} />
            </Field>
            <Field label="canon version">{event.canon_version}</Field>
          </dl>
        </Section>

        <Section title="Payload">
          <dl>
            <Field label="request digest">
              <Hash value={emitted.request_digest} />
            </Field>
            <Field label="response digest">
              <Hash value={emitted.response_digest} />
            </Field>
          </dl>
          <p className="mt-2 text-xs leading-relaxed text-[var(--color-muted)]">
            Digests, never bodies. A prompt or completion is personal data and
            this table has a seven-year retention; a 32-byte hash is not.
          </p>
        </Section>

        <Section title="Metrics">
          <dl>
            <Field label="provider">{emitted.metrics.provider || "—"}</Field>
            <Field label="http status">{emitted.metrics.http_status}</Field>
            <Field label="latency">{emitted.metrics.latency_ms} ms</Field>
            <Field label="ttft">{emitted.metrics.ttft_ms} ms</Field>
            <Field label="tokens in / out">
              {emitted.metrics.tokens_in} / {emitted.metrics.tokens_out}
            </Field>
          </dl>
        </Section>

        <Section title="Origin">
          <dl>
            <Field label="occurred at">
              {formatInstant(emitted.occurred_at)}
            </Field>
            <Field label="node">{emitted.node_id}</Field>
            <Field label="trace">
              {emitted.trace_id ? <Hash value={emitted.trace_id} /> : "—"}
            </Field>
          </dl>
        </Section>
      </div>
    </Card>
  );
}

function Section({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <div className="border-t border-[var(--color-line-soft)] py-3 first:border-0 first:pt-0">
      <div className="mb-1 text-xs font-medium uppercase tracking-wider text-[var(--color-faint)]">
        {title}
      </div>
      {children}
    </div>
  );
}

/**
 * Contiguous runs covered by a checkpoint, merged where they abut.
 *
 * Mirrors `attested_runs` in the verifier, and for the same reason: "seq 1–20
 * and 24–30 are signed" is a materially different statement from "26 of 30
 * events are signed", and only the first tells anyone where to look.
 *
 * It reports *coverage*, not validity — nothing here checks a signature.
 */
function coveredRuns(checkpoints: Checkpoint[]): Array<[number, number]> {
  const ranges = checkpoints
    .map((c) => [c.body.seq_from, c.body.seq_to] as [number, number])
    .sort((a, b) => a[0] - b[0] || a[1] - b[1]);

  const runs: Array<[number, number]> = [];
  for (const [from, to] of ranges) {
    const last = runs[runs.length - 1];
    if (last && from <= last[1] + 1) last[1] = Math.max(last[1], to);
    else runs.push([from, to]);
  }
  return runs;
}
