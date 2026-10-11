import { useState, type SyntheticEvent } from "react";
import { Navigate, useNavigate } from "react-router";

import { client, describeError, unwrap } from "../../api";
import { Brand, Button, ErrorState, InlineError, LoadingState, TextField } from "../../ui";
import { useSession } from "../session";
import { useSetupStatus } from "./useSetupStatus";

export function SetupPage() {
  const status = useSetupStatus();
  const { adopt } = useSession();
  const navigate = useNavigate();
  const [token, setToken] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ReturnType<typeof describeError> | null>(null);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      const session = await unwrap(
        client.POST("/api/v1/setup", { body: { token: token.trim(), email: email.trim(), password } }),
      );
      adopt(session);
      void navigate("/w", { replace: true });
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  if (status.status === "ready" && !status.data.required) {
    return <Navigate to="/sign-in" replace />;
  }

  return (
    <main className="centered-page" id="main">
      <Brand size={36} />
      <div className="panel card">
        {status.status === "loading" ? <LoadingState label="Checking this instance" /> : null}
        {status.status === "error" ? <ErrorState {...describeError(status.error)} onRetry={status.reload} /> : null}
        {status.status === "ready" ? (
          <>
            <div>
              <h1 className="card-title">Set up zirv flow</h1>
              <p className="card-lede">
                Create the owner account for this instance. The server logs a one-time setup token when it starts with no owner.
              </p>
            </div>
            <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
              <TextField
                label="Setup token"
                mono
                autoComplete="off"
                spellCheck={false}
                value={token}
                onChange={(event) => setToken(event.target.value)}
                required
              />
              <TextField
                label="Owner email"
                type="email"
                autoComplete="username"
                value={email}
                onChange={(event) => setEmail(event.target.value)}
                required
              />
              <TextField
                label="Password"
                type="password"
                autoComplete="new-password"
                value={password}
                onChange={(event) => setPassword(event.target.value)}
                required
              />
              {failure ? <InlineError {...failure} /> : null}
              <Button type="submit" variant="primary" large disabled={busy}>
                {busy ? "Creating owner" : "Create owner and sign in"}
              </Button>
            </form>
          </>
        ) : null}
      </div>
    </main>
  );
}
