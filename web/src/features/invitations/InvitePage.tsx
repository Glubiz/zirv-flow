import { useEffect, useState, type ReactNode, type SyntheticEvent } from "react";
import { useNavigate, useParams } from "react-router";

import { ApiError, client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Brand, Button, EmptyState, ErrorState, InlineError, LoadingState, TextField } from "../../ui";
import { useSession } from "../session";
import { clearPendingInvite, rememberInvite } from "./pendingInvite";

const ROLE_LABELS: Record<string, string> = { admin: "Admin", publisher: "Publisher", editor: "Editor", viewer: "Viewer" };
const MISMATCH = "invitation_address_mismatch";

/** Whether `email` can be the address behind a hint such as `c***@acme.com`. */
function matchesHint(email: string, hint: string): boolean {
  const [start = "", domain = ""] = hint.split("***@");
  const lower = email.toLowerCase();
  return lower.startsWith(start.toLowerCase()) && lower.endsWith(`@${domain.toLowerCase()}`);
}

export function InvitePage() {
  const { token = "" } = useParams();
  return <Invitation key={token} token={token} />;
}

function Shell({ children }: { children: ReactNode }) {
  return (
    <main className="centered-page" id="main">
      <Brand size={36} />
      <div className="panel card">{children}</div>
    </main>
  );
}

function Invitation({ token }: { token: string }) {
  const { state, signOut, reload } = useSession();
  const navigate = useNavigate();
  const invitation = useResource(() => unwrap(client.GET("/api/v1/invitations/{token}", { params: { path: { token } } })), [token]);
  const methods = useResource(() => unwrap(client.GET("/api/v1/session/methods")), []);
  // Single sign-on may have brought the person back here; the waiting token has done its job.
  useEffect(() => clearPendingInvite(), []);
  const [creating, setCreating] = useState(false);
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  if (invitation.status === "loading" || state.status === "loading") {
    return (
      <Shell>
        <LoadingState label="Opening your invitation" />
      </Shell>
    );
  }
  if (invitation.status === "error") {
    if (invitation.error instanceof ApiError && invitation.error.status === 404) {
      clearPendingInvite();
      return (
        <Shell>
          <EmptyState title="This invitation is no longer valid">Ask the person who invited you for a new link.</EmptyState>
        </Shell>
      );
    }
    return (
      <Shell>
        <ErrorState {...describeError(invitation.error)} onRetry={invitation.reload} />
      </Shell>
    );
  }

  const { workspace_name: workspace, role, email_hint: hint } = invitation.data;
  const user = state.status === "signed-in" ? state.user : null;
  const wrongAddress = user !== null && !matchesHint(user.email, hint);
  const local = methods.status === "ready" && methods.data.local;

  async function accept(body: { password?: string }, created: boolean) {
    setBusy(true);
    setFailure(null);
    try {
      const accepted = await unwrap(
        client.POST("/api/v1/invitations/{token}/accept", { params: { path: { token } }, body }),
      );
      clearPendingInvite();
      if (created) reload();
      void navigate(`/w/${accepted.workspace_id}`, { replace: true });
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) {
        invitation.reload();
        return;
      }
      setFailure(error instanceof ApiError && error.code === MISMATCH ? mismatch() : describeError(error));
      setBusy(false);
    }
  }

  function signIn() {
    rememberInvite(token);
    void navigate("/sign-in", { state: { from: `/invite/${token}` } });
  }

  function submitAccount(event: SyntheticEvent) {
    event.preventDefault();
    void accept({ password }, true);
  }

  return (
    <Shell>
      <div>
        <h1 className="card-title">Join {workspace}</h1>
        <p className="card-lede">
          {ROLE_LABELS[role] ?? role} · {hint}
        </p>
      </div>
      {wrongAddress ? (
        <>
          <InlineError {...mismatch()} />
          <Button onClick={() => void signOut()}>Sign out</Button>
        </>
      ) : user ? (
        <>
          {failure ? <InlineError {...failure} /> : null}
          {failure?.title === mismatch().title ? <Button onClick={() => void signOut()}>Sign out</Button> : null}
          <Button variant="primary" large disabled={busy} onClick={() => void accept({}, false)}>
            {busy ? "Joining" : "Join"}
          </Button>
        </>
      ) : creating ? (
        <form className="stack" onSubmit={submitAccount} noValidate>
          <TextField
            label="Password"
            type="password"
            autoComplete="new-password"
            hint="12 to 1024 characters."
            value={password}
            onChange={(event) => setPassword(event.target.value)}
            required
          />
          {failure ? <InlineError {...failure} /> : null}
          <Button type="submit" variant="primary" large disabled={busy || password === ""}>
            {busy ? "Creating" : "Create account"}
          </Button>
        </form>
      ) : (
        <>
          <Button variant="primary" large onClick={signIn}>
            Sign in to join
          </Button>
          {local ? <Button onClick={() => setCreating(true)}>Create an account</Button> : null}
        </>
      )}
    </Shell>
  );
}

function mismatch(): ErrorSummary {
  return {
    title: "This invitation is for another address",
    detail: "Sign out and use the account it was sent to.",
    traceId: null,
  };
}
