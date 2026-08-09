import { useState, type FormEvent } from "react";
import { KeyRound } from "lucide-react";
import { session } from "@/lib/api";
import { Button, Card, Input } from "@/components/ui";

export function Login({ onDone }: { onDone: () => void }) {
  const [token, setToken] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await session.login(token);
      onDone();
    } catch {
      // The server does not say why, and neither does this. "Wrong token" and
      // "no such operator" are the same answer, or the login box becomes an
      // oracle for guessing.
      setError("That token was not accepted.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex min-h-dvh items-center justify-center px-6">
      <div className="w-full max-w-sm">
        <div className="mb-8 text-center">
          <div className="text-lg font-semibold tracking-tight">Ancre</div>
          <p className="mt-1.5 text-sm text-[var(--color-muted)]">
            Evidence for AI systems that serve real traffic
          </p>
        </div>

        <Card>
          <form onSubmit={submit} className="space-y-4 p-5">
            <div>
              <label
                htmlFor="token"
                className="mb-1.5 flex items-center gap-1.5 text-xs font-medium text-[var(--color-muted)]"
              >
                <KeyRound className="size-3.5" />
                Operator token
              </label>
              <Input
                id="token"
                type="password"
                autoFocus
                autoComplete="off"
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder="ANCRE_ADMIN_TOKEN"
              />
            </div>

            {error ? (
              <p className="text-xs text-[var(--color-bad)]">{error}</p>
            ) : null}

            <Button
              type="submit"
              variant="primary"
              size="md"
              className="w-full"
              disabled={busy || token.length === 0}
            >
              {busy ? "Checking…" : "Sign in"}
            </Button>
          </form>
        </Card>

        <p className="mt-5 text-center text-xs leading-relaxed text-[var(--color-faint)]">
          Set <code className="text-[var(--color-muted)]">ANCRE_ADMIN_TOKEN</code>{" "}
          on the control plane. If it is unset, a token is generated at startup
          and printed once in the logs.
        </p>
      </div>
    </div>
  );
}
