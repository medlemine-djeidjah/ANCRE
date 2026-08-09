/**
 * The primitives. Deliberately few.
 *
 * A dashboard whose job is to make evidence legible does not need a component
 * library's worth of surface. Six things, each with one obvious use, is easier
 * to keep visually consistent than thirty with overlapping ones — and every
 * variant below corresponds to a distinction the data actually makes.
 */
import { cva, type VariantProps } from "class-variance-authority";
import type { ReactNode } from "react";
import { cn } from "@/lib/format";

// --- surfaces ---------------------------------------------------------------

export function Card({
  className,
  children,
}: {
  className?: string;
  children: ReactNode;
}) {
  return (
    <div
      className={cn(
        "rounded-[var(--radius-card)] border border-[var(--color-line)]",
        "bg-[var(--color-surface)]",
        className,
      )}
    >
      {children}
    </div>
  );
}

export function CardHeader({
  title,
  hint,
  action,
}: {
  title: ReactNode;
  hint?: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="flex items-start justify-between gap-4 border-b border-[var(--color-line-soft)] px-5 py-4">
      <div className="min-w-0">
        <h2 className="text-sm font-semibold tracking-tight">{title}</h2>
        {hint ? (
          <p className="mt-1 text-xs leading-relaxed text-[var(--color-muted)]">
            {hint}
          </p>
        ) : null}
      </div>
      {action ? <div className="shrink-0">{action}</div> : null}
    </div>
  );
}

/** A labelled number. The unit of a summary. */
export function Stat({
  label,
  value,
  tone = "plain",
  hint,
}: {
  label: string;
  value: ReactNode;
  tone?: "plain" | "ok" | "warn" | "bad";
  hint?: string;
}) {
  const colour = {
    plain: "text-[var(--color-ink)]",
    ok: "text-[var(--color-ok)]",
    warn: "text-[var(--color-warn)]",
    bad: "text-[var(--color-bad)]",
  }[tone];

  return (
    <div className="px-5 py-4">
      <div className="text-xs font-medium uppercase tracking-wider text-[var(--color-faint)]">
        {label}
      </div>
      <div className={cn("tnum mt-1.5 text-2xl font-semibold tracking-tight", colour)}>
        {value}
      </div>
      {hint ? (
        <div className="mt-1 text-xs text-[var(--color-muted)]">{hint}</div>
      ) : null}
    </div>
  );
}

// --- badges -----------------------------------------------------------------

const badge = cva(
  "inline-flex items-center gap-1.5 rounded-md border px-2 py-0.5 text-xs font-medium whitespace-nowrap",
  {
    variants: {
      tone: {
        neutral:
          "border-[var(--color-line)] bg-[var(--color-raised)] text-[var(--color-muted)]",
        ok: "border-[color-mix(in_oklch,var(--color-ok)_35%,transparent)] bg-[color-mix(in_oklch,var(--color-ok)_12%,transparent)] text-[var(--color-ok)]",
        warn: "border-[color-mix(in_oklch,var(--color-warn)_35%,transparent)] bg-[color-mix(in_oklch,var(--color-warn)_12%,transparent)] text-[var(--color-warn)]",
        bad: "border-[color-mix(in_oklch,var(--color-bad)_40%,transparent)] bg-[color-mix(in_oklch,var(--color-bad)_12%,transparent)] text-[var(--color-bad)]",
        accent:
          "border-[color-mix(in_oklch,var(--color-accent)_35%,transparent)] bg-[color-mix(in_oklch,var(--color-accent)_12%,transparent)] text-[var(--color-accent)]",
      },
    },
    defaultVariants: { tone: "neutral" },
  },
);

export function Badge({
  tone,
  className,
  children,
  title,
}: VariantProps<typeof badge> & {
  className?: string;
  children: ReactNode;
  title?: string;
}) {
  return (
    <span className={cn(badge({ tone }), className)} title={title}>
      {children}
    </span>
  );
}

/**
 * Risk class, coloured by what it *does* rather than by how alarming it sounds.
 *
 * `high` is the only class that changes the gateway's behaviour — it is the one
 * that fails closed on stale configuration — so it is the only one that gets a
 * colour. Colouring all four would make the palette decorative and leave a
 * reader no way to tell which distinction matters.
 */
export function RiskClass({ value }: { value: string }) {
  return (
    <Badge
      tone={value === "high" ? "warn" : "neutral"}
      title={
        value === "high"
          ? "High risk: this system is refused rather than served on stale configuration"
          : `Risk class: ${value}`
      }
    >
      {value}
    </Badge>
  );
}

/** A risk flag. Every one is a finding, so every one is coloured. */
export function RiskFlag({ name }: { name: string }) {
  const meaning: Record<string, string> = {
    unpinned_model:
      "The provider answered with a floating alias, so which weights ran cannot be established",
    pin_overridden: "A caller overrode a pin from a request header",
    stale_config:
      "Served under a configuration older than the staleness budget",
    telemetry_dropped:
      "Events were lost in this window. The chain is complete; the record is not",
    no_policy_engine: "No policy engine is configured",
    substantial_candidate:
      "This configuration change may constitute a substantial modification. Review required",
  };
  return (
    <Badge tone="warn" title={meaning[name] ?? name}>
      {name.replace(/_/g, " ")}
    </Badge>
  );
}

// --- controls ---------------------------------------------------------------

const button = cva(
  "inline-flex items-center justify-center gap-2 rounded-lg text-sm font-medium transition-colors disabled:pointer-events-none disabled:opacity-50",
  {
    variants: {
      variant: {
        primary:
          "bg-[var(--color-accent)] text-[var(--color-bg)] hover:opacity-90",
        secondary:
          "border border-[var(--color-line)] bg-[var(--color-raised)] text-[var(--color-ink)] hover:border-[var(--color-faint)]",
        ghost: "text-[var(--color-muted)] hover:text-[var(--color-ink)]",
      },
      size: {
        sm: "h-8 px-3",
        md: "h-9 px-4",
      },
    },
    defaultVariants: { variant: "secondary", size: "sm" },
  },
);

export function Button({
  variant,
  size,
  className,
  ...props
}: VariantProps<typeof button> &
  React.ButtonHTMLAttributes<HTMLButtonElement>) {
  return <button className={cn(button({ variant, size }), className)} {...props} />;
}

export function Input({
  className,
  ...props
}: React.InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      className={cn(
        "h-9 w-full rounded-lg border border-[var(--color-line)] bg-[var(--color-bg)] px-3 text-sm",
        "text-[var(--color-ink)] placeholder:text-[var(--color-faint)]",
        className,
      )}
      {...props}
    />
  );
}

// --- text -------------------------------------------------------------------

/** A hash, truncated in the middle, copyable in full. */
export function Hash({
  value,
  keep = 6,
  className,
}: {
  value: string;
  keep?: number;
  className?: string;
}) {
  return (
    <button
      type="button"
      title={`${value}\n\nClick to copy`}
      onClick={() => void navigator.clipboard?.writeText(value)}
      className={cn(
        "hash text-[var(--color-muted)] hover:text-[var(--color-ink)]",
        className,
      )}
    >
      {value.length <= keep * 2 + 1
        ? value
        : `${value.slice(0, keep)}…${value.slice(-keep)}`}
    </button>
  );
}

/** A key/value line, for the detail panels. */
export function Field({
  label,
  children,
  tone,
}: {
  label: string;
  children: ReactNode;
  tone?: "warn";
}) {
  return (
    <div className="flex items-baseline justify-between gap-4 py-1.5">
      <dt className="shrink-0 text-xs text-[var(--color-faint)]">{label}</dt>
      <dd
        className={cn(
          "min-w-0 truncate text-right text-[13px]",
          tone === "warn"
            ? "text-[var(--color-warn)]"
            : "text-[var(--color-ink)]",
        )}
      >
        {children}
      </dd>
    </div>
  );
}

export function Empty({ children }: { children: ReactNode }) {
  return (
    <div className="px-5 py-12 text-center text-sm text-[var(--color-muted)]">
      {children}
    </div>
  );
}
