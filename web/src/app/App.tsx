import { Navigate, Route, Routes, useLocation } from "react-router";

import { ApiKeysPage } from "../features/api-keys";
import { EventSchemaPage, EventSettingsPage, EventsPage, RejectsPage } from "../features/events";
import { HealthPage } from "../features/health";
import { JobsPage } from "../features/jobs";
import { ListDetailPage, ListsPage } from "../features/lists";
import { MembersPage } from "../features/members";
import { SuppressionsPage, TopicsPage } from "../features/consent";
import { ConflictsPage, PeoplePage, ProfilePage, TraitsPage } from "../features/people";
import { PrivacyPage } from "../features/privacy";
import { TracesPage } from "../features/traces";
import { SessionProvider, SignInPage, useSession } from "../features/session";
import { SetupPage, useSetupStatus } from "../features/setup";
import { WorkspacesPage, WorkspacesProvider } from "../features/workspaces";
import { EmptyState, ErrorState, LoadingState, SkipLink } from "../ui";
import { describeError } from "../api";
import { Shell } from "./Shell";

function Centered({ children }: { children: React.ReactNode }) {
  return (
    <main className="centered-page" id="main">
      {children}
    </main>
  );
}

function SignedOutRedirect() {
  const setup = useSetupStatus();
  const location = useLocation();

  if (setup.status === "loading") {
    return (
      <Centered>
        <LoadingState label="Loading" />
      </Centered>
    );
  }
  if (setup.status === "ready" && setup.data.required) {
    return <Navigate to="/setup" replace />;
  }
  return <Navigate to="/sign-in" replace state={{ from: `${location.pathname}${location.search}` }} />;
}

function Protected() {
  const { state, reload } = useSession();

  switch (state.status) {
    case "loading":
      return (
        <Centered>
          <LoadingState label="Loading your session" />
        </Centered>
      );
    case "error":
      return (
        <Centered>
          <div className="panel card">
            <ErrorState {...describeError(state.error)} onRetry={reload} />
          </div>
        </Centered>
      );
    case "signed-out":
      return <SignedOutRedirect />;
    case "signed-in":
      return (
        <WorkspacesProvider>
          <Shell />
        </WorkspacesProvider>
      );
  }
}

function SignInRoute() {
  const { state } = useSession();
  const location = useLocation();

  if (state.status === "loading") {
    return (
      <Centered>
        <LoadingState label="Loading your session" />
      </Centered>
    );
  }
  if (state.status === "signed-in") {
    const from = (location.state as { from?: string } | null)?.from ?? "/w";
    return <Navigate to={from} replace />;
  }
  return <SignInPage />;
}

function NotFound() {
  return (
    <div className="page">
      <div className="panel panel-flush">
        <EmptyState title="This page does not exist">Use the menu above to go back to your workspaces.</EmptyState>
      </div>
    </div>
  );
}

export function AppRoutes() {
  return (
    <SessionProvider>
      <SkipLink target="main" />
      <Routes>
        <Route path="/setup" element={<SetupPage />} />
        <Route path="/sign-in" element={<SignInRoute />} />
        <Route element={<Protected />}>
          <Route index element={<Navigate to="/w" replace />} />
          <Route path="w" element={<WorkspacesPage />} />
          <Route path="w/:ws" element={<Navigate to="members" replace />} />
          <Route path="w/:ws/members" element={<MembersPage />} />
          <Route path="w/:ws/traits" element={<TraitsPage />} />
          <Route path="w/:ws/lists" element={<ListsPage />} />
          <Route path="w/:ws/lists/:listId" element={<ListDetailPage />} />
          <Route path="w/:ws/people" element={<PeoplePage />} />
          <Route path="w/:ws/people/conflicts" element={<ConflictsPage />} />
          <Route path="w/:ws/people/:id" element={<ProfilePage />} />
          <Route path="w/:ws/events" element={<EventsPage />} />
          <Route path="w/:ws/events/rejects" element={<RejectsPage />} />
          <Route path="w/:ws/events/settings" element={<EventSettingsPage />} />
          <Route path="w/:ws/events/schemas/:name" element={<EventSchemaPage />} />
          <Route path="w/:ws/topics" element={<TopicsPage />} />
          <Route path="w/:ws/suppressions" element={<SuppressionsPage />} />
          <Route path="w/:ws/privacy" element={<PrivacyPage />} />
          <Route path="w/:ws/api-keys" element={<ApiKeysPage />} />
          <Route path="w/:ws/traces" element={<TracesPage />} />
          <Route path="w/:ws/traces/:traceId" element={<TracesPage />} />
          <Route path="jobs" element={<JobsPage />} />
          <Route path="health" element={<HealthPage />} />
          <Route path="*" element={<NotFound />} />
        </Route>
      </Routes>
    </SessionProvider>
  );
}
