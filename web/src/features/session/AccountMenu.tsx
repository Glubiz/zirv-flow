import { useState } from "react";
import { useNavigate } from "react-router";

import { describeError } from "../../api";
import { Button, Dialog, InlineError, MenuButton } from "../../ui";
import { useSession, useSignedInUser } from "./SessionProvider";

function initials(email: string): string {
  const local = email.split("@")[0] ?? email;
  return local.slice(0, 2).toUpperCase();
}

export function AccountMenu() {
  const user = useSignedInUser();
  const { signOut } = useSession();
  const navigate = useNavigate();
  const [failure, setFailure] = useState<ReturnType<typeof describeError> | null>(null);

  async function leave() {
    try {
      await signOut();
      void navigate("/sign-in", { replace: true });
    } catch (error) {
      setFailure(describeError(error));
    }
  }

  return (
    <>
      <MenuButton
        label="Account menu"
        buttonClassName="avatar-button"
        buttonContent={initials(user.email)}
        heading={user.email}
        entries={[{ key: "sign-out", label: "Sign out", onSelect: () => void leave() }]}
      />
      {failure ? (
        <Dialog title="Could not sign out" onClose={() => setFailure(null)}>
          <InlineError {...failure} />
          <div className="dialog-actions">
            <Button variant="primary" onClick={() => setFailure(null)}>
              Close
            </Button>
          </div>
        </Dialog>
      ) : null}
    </>
  );
}
