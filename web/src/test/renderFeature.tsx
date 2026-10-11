import { render } from "@testing-library/react";
import type { ReactElement, ReactNode } from "react";
import { MemoryRouter, Route, Routes } from "react-router";

import { SessionProvider, useSession } from "../features/session";

function WhenSignedIn({ children }: { children: ReactNode }) {
  const { state } = useSession();
  return state.status === "signed-in" ? <>{children}</> : <p>Waiting for the session</p>;
}

export function renderRoute(path: string, pattern: string, element: ReactElement) {
  return render(
    <SessionProvider>
      <MemoryRouter initialEntries={[path]}>
        <WhenSignedIn>
          <Routes>
            <Route path={pattern} element={element} />
          </Routes>
        </WhenSignedIn>
      </MemoryRouter>
    </SessionProvider>,
  );
}
