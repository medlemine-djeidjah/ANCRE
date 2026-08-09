import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/** Timestamps on the wire are integer microseconds — see `Timestamp`. */
export function fromMicros(micros: number): Date {
  return new Date(micros / 1000);
}

export function formatInstant(micros: number | null | undefined): string {
  if (micros == null) return "—";
  return fromMicros(micros).toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

export function formatTime(micros: number): string {
  return fromMicros(micros).toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

/**
 * Thin-space grouping, matching the verifier's own output. A seven-digit event
 * count is read wrong often enough to be worth the function.
 */
export function thousands(n: number): string {
  return n.toString().replace(/\B(?=(\d{3})+(?!\d))/g, " ");
}

/**
 * Both ends of a hash, because that is what people compare. Never one end:
 * a prefix-only rendering makes two different hashes look identical in exactly
 * the situation where telling them apart matters.
 */
export function shortHash(hex: string, keep = 6): string {
  if (hex.length <= keep * 2 + 1) return hex;
  return `${hex.slice(0, keep)}…${hex.slice(-keep)}`;
}

/** Human duration from a microsecond span. */
export function formatSpan(fromMicro: number, toMicro: number): string {
  const seconds = Math.max(0, Math.round((toMicro - fromMicro) / 1_000_000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.round(minutes / 60);
  if (hours < 48) return `${hours}h`;
  return `${Math.round(hours / 24)}d`;
}

/**
 * The pins that count as a gap: `unknown`, or a `unresolved:` alias.
 *
 * This mirrors `Pins::has_gap` in `ancre-types`. It is duplicated here on
 * purpose and it is the *display* copy — the number that matters comes from
 * the server's `events_with_gaps`, which mirrors the same rule in SQL. If the
 * two ever disagree the summary is what to believe, and the disagreement is a
 * bug worth chasing.
 */
export const GAP_PINS = [
  "system_version",
  "ifu_version",
  "model_id",
  "model_version",
  "prompt_id",
  "prompt_version",
  "policy_id",
  "policy_version",
  "gateway_version",
] as const;

export function isGap(value: string): boolean {
  return value === "unknown" || value.startsWith("unresolved:");
}

/** Save a set of files as individual downloads. */
export function downloadFiles(prefix: string, files: Record<string, string>) {
  for (const [name, content] of Object.entries(files)) {
    const url = URL.createObjectURL(
      new Blob([content], { type: "application/octet-stream" }),
    );
    const a = document.createElement("a");
    a.href = url;
    a.download = `${prefix}-${name}`;
    document.body.appendChild(a);
    a.click();
    a.remove();
    URL.revokeObjectURL(url);
  }
}
