import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";

import { ApiError, client, setCsrfToken, setUnauthorizedHandler, unwrap, unwrapEmpty, type components } from "../../api";

export type SessionUser = components["schemas"]["UserBody"];
type SessionBody = components["schemas"]["SessionBody"];

export type SessionState =
  | { status: "loading" }
  | { status: "signed-out" }
  | { status: "error"; error: unknown }
  | { status: "signed-in"; user: SessionUser };

interface SessionApi {
  state: SessionState;
  signIn: (email: string, password: string) => Promise<void>;
  signOut: () => Promise<void>;
  adopt: (body: SessionBody) => void;
  reload: () => void;
}

const SessionContext = createContext<SessionApi | null>(null);

export function SessionProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<SessionState>({ status: "loading" });
  const [tick, setTick] = useState(0);

  const adopt = useCallback((body: SessionBody) => {
    setCsrfToken(body.csrf_token);
    setState({ status: "signed-in", user: body.user });
  }, []);

  useEffect(() => {
    let live = true;
    client.GET("/api/v1/session").then(
      ({ data, response }) => {
        if (!live) return;
        if (response.ok && data) {
          adopt(data);
        } else if (response.status === 401) {
          setCsrfToken(null);
          setState({ status: "signed-out" });
        } else {
          setState({ status: "error", error: new ApiError({ title: response.statusText || "Request failed", detail: "", status: response.status, traceId: null, requestId: null }) });
        }
      },
      (error: unknown) => {
        if (live) setState({ status: "error", error });
      },
    );
    return () => {
      live = false;
    };
  }, [adopt, tick]);

  useEffect(() => {
    setUnauthorizedHandler(() => {
      setCsrfToken(null);
      setState((current) => (current.status === "signed-in" ? { status: "signed-out" } : current));
    });
    return () => setUnauthorizedHandler(null);
  }, []);

  const signIn = useCallback(
    async (email: string, password: string) => {
      adopt(await unwrap(client.POST("/api/v1/session", { body: { email, password } })));
    },
    [adopt],
  );

  const signOut = useCallback(async () => {
    try {
      await unwrapEmpty(client.DELETE("/api/v1/session"));
    } catch (error) {
      if (!(error instanceof ApiError) || error.status !== 401) {
        throw error;
      }
    }
    setCsrfToken(null);
    setState({ status: "signed-out" });
  }, []);

  const reload = useCallback(() => {
    setState({ status: "loading" });
    setTick((value) => value + 1);
  }, []);

  const value = useMemo(() => ({ state, signIn, signOut, adopt, reload }), [state, signIn, signOut, adopt, reload]);
  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>;
}

export function useSession(): SessionApi {
  const value = useContext(SessionContext);
  if (!value) {
    throw new Error("useSession needs a SessionProvider");
  }
  return value;
}

export function useSignedInUser(): SessionUser {
  const { state } = useSession();
  if (state.status !== "signed-in") {
    throw new Error("useSignedInUser needs a signed-in session");
  }
  return state.user;
}
