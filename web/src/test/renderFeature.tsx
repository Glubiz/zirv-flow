import { render } from "@testing-library/react";
import type { ReactElement, ReactNode } from "react";
import { createMemoryRouter, Route, RouterProvider, Routes } from "react-router";

import { SessionProvider, useSession } from "../features/session";

function WhenSignedIn({ children }: { children: ReactNode }) {
  const { state } = useSession();
  return state.status === "signed-in" ? <>{children}</> : <p>Waiting for the session</p>;
}

/** A data router around `element`, as the app has, so blockers and history work as they do there. */
export function renderRoute(path: string, pattern: string, element: ReactElement, history: string[] = [path]) {
  const router = createMemoryRouter(
    [
      {
        path: "*",
        element: (
          <SessionProvider>
            <WhenSignedIn>
              <Routes>
                <Route path={pattern} element={element} />
              </Routes>
            </WhenSignedIn>
          </SessionProvider>
        ),
      },
    ],
    { initialEntries: history, initialIndex: history.indexOf(path) },
  );
  return { ...render(<RouterProvider router={router} />), router };
}
