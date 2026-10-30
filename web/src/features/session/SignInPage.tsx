import { useState, type SyntheticEvent } from "react";
import { useLocation, useNavigate } from "react-router";

import { client, describeError, unwrap, useResource } from "../../api";
import { Brand, Button, EmptyState, InlineError, LoadingState, TextField } from "../../ui";
import { useSession } from "./SessionProvider";

interface FromState {
  from?: string;
}

const BOTH_METHODS = { local: true, oidc: true };

export function SignInPage() {
  const { signIn } = useSession();
  const navigate = useNavigate();
  const location = useLocation();
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ReturnType<typeof describeError> | null>(null);
  const version = useResource(() => unwrap(client.GET("/api/v1/version")), []);
  const methodsResource = useResource(() => unwrap(client.GET("/api/v1/session/methods")), []);

  const from = (location.state as FromState | null)?.from ?? "/";
  const methods = methodsResource.status === "ready" ? methodsResource.data : BOTH_METHODS;
  const settled = methodsResource.status !== "loading";

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      await signIn(email.trim(), password);
      void navigate(from, { replace: true });
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  const host = globalThis.location.hostname || "zirv flow";
  const footnote = version.status === "ready" ? `${host} · v${version.data.version}` : host;

  return (
    <main className="centered-page" id="main">
      <Brand size={36} />
      <div className="panel card">
        <div>
          <h1 className="card-title">Welcome back</h1>
          <p className="card-lede">Sign in to keep your segments and flows moving.</p>
        </div>
        {settled ? null : <LoadingState label="Checking how you can sign in" />}
        {settled && !methods.local && !methods.oidc ? (
          <EmptyState title="Sign-in is turned off">This instance accepts no sign-in method. Ask the person who runs it.</EmptyState>
        ) : null}
        {settled && methods.oidc ? (
          <a className="btn btn-large" href="/api/v1/session/oidc/start">
            Continue with single sign-on
          </a>
        ) : null}
        {settled && methods.oidc && methods.local ? <div className="divider">or use your email</div> : null}
        {settled && methods.local ? (
          <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
            <TextField
              label="Email"
              type="email"
              autoComplete="username"
              value={email}
              onChange={(event) => setEmail(event.target.value)}
              required
            />
            <TextField
              label="Password"
              type="password"
              autoComplete="current-password"
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              required
            />
            {failure ? <InlineError {...failure} /> : null}
            <Button type="submit" variant="primary" large disabled={busy}>
              {busy ? "Signing in" : "Sign in"}
            </Button>
          </form>
        ) : null}
      </div>
      <p className="footnote">{footnote}</p>
    </main>
  );
}
