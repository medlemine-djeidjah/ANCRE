import { useCallback, useEffect, useState } from "react";
import { BrowserRouter, Link, Route, Routes } from "react-router";
import { LogOut, Moon, Sun } from "lucide-react";
import { session, Unauthenticated } from "@/lib/api";
import { Button } from "@/components/ui";
import { Login } from "@/routes/Login";
import { Chains } from "@/routes/Chains";
import { Chain } from "@/routes/Chain";

export function App() {
  const [authed, setAuthed] = useState<boolean | null>(null);
  const [dark, setDark] = useState(true);

  const check = useCallback(() => {
    session
      .status()
      .then((s) => setAuthed(s.authenticated))
      .catch(() => setAuthed(false));
  }, []);

  useEffect(check, [check]);

  useEffect(() => {
    document.documentElement.classList.toggle("dark", dark);
  }, [dark]);

  // A session can expire while the tab is open. Rather than let every screen
  // handle its own 401, one listener flips the whole app back to the login
  // screen — the state is global because the cause is.
  useEffect(() => {
    const onRejection = (e: PromiseRejectionEvent) => {
      if (e.reason instanceof Unauthenticated) setAuthed(false);
    };
    window.addEventListener("unhandledrejection", onRejection);
    return () => window.removeEventListener("unhandledrejection", onRejection);
  }, []);

  if (authed === null) return <div className="min-h-dvh" />;
  if (!authed) return <Login onDone={check} />;

  return (
    <BrowserRouter>
      <div className="min-h-dvh">
        <nav className="border-b border-[var(--color-line)] bg-[var(--color-surface)]">
          <div className="mx-auto flex max-w-6xl items-center justify-between px-6 py-3">
            <Link to="/" className="text-sm font-semibold tracking-tight">
              Ancre
            </Link>
            <div className="flex items-center gap-1">
              <Button
                variant="ghost"
                onClick={() => setDark((d) => !d)}
                title={dark ? "Switch to light" : "Switch to dark"}
              >
                {dark ? <Sun className="size-4" /> : <Moon className="size-4" />}
              </Button>
              <Button
                variant="ghost"
                onClick={() => void session.logout().then(() => setAuthed(false))}
                title="Sign out"
              >
                <LogOut className="size-4" />
              </Button>
            </div>
          </div>
        </nav>

        <Routes>
          <Route path="/" element={<Chains />} />
          <Route path="/chains/:tenant/:system" element={<Chain />} />
          <Route path="*" element={<Chains />} />
        </Routes>
      </div>
    </BrowserRouter>
  );
}
