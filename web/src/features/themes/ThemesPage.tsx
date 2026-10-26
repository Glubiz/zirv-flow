import { useState, type SyntheticEvent } from "react";
import { Link, useNavigate, useParams } from "react-router";

import { ApiError, client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, EmptyState, InlineError, LibraryNav, PageHeader, Pill, ResourceView, TextField } from "../../ui";
import { plural } from "../../ui/format";
import { useThemeAccess } from "./access";
import { readTokens, isPublished, type ThemeListItem } from "./model";
import { ArchiveDialog, SetDefaultDialog, ThemeMenu } from "./ThemeActions";
import { ThemeThumb } from "./ThemeThumb";
import { useThemes } from "./useThemes";

export function ThemesPage() {
  const { ws = "" } = useParams();
  return <ThemeList key={ws} workspaceId={ws} />;
}

function ThemeList({ workspaceId }: { workspaceId: string }) {
  const access = useThemeAccess(workspaceId);
  const [adding, setAdding] = useState(false);
  const [defaulting, setDefaulting] = useState<ThemeListItem | null>(null);
  const [archiving, setArchiving] = useState<ThemeListItem | null>(null);
  const themes = useThemes(workspaceId);

  return (
    <div className="page">
      <LibraryNav workspaceId={workspaceId} />
      <PageHeader
        title="Themes"
        actions={
          access.canWrite ? (
            <Button variant="primary" onClick={() => setAdding(true)}>
              New theme
            </Button>
          ) : undefined
        }
      />
      <ResourceView
        resource={themes}
        loadingLabel="Loading themes"
        isEmpty={(data) => data.length === 0}
        empty={
          <EmptyState
            title="No themes yet"
            action={
              access.canWrite ? (
                <Button variant="primary" onClick={() => setAdding(true)}>
                  New theme
                </Button>
              ) : undefined
            }
          >
            {access.canWrite ? "Make a theme to give your emails one look." : "Members of the workspace make the themes."}
          </EmptyState>
        }
      >
        {(data) => (
          <>
            <ul className="th-grid" aria-label="Themes of this workspace">
              {data.map((theme) => (
                <ThemeCard
                  key={theme.id}
                  workspaceId={workspaceId}
                  theme={theme}
                  canAdmin={access.canAdmin}
                  onSetDefault={() => setDefaulting(theme)}
                  onArchive={() => setArchiving(theme)}
                />
              ))}
            </ul>
            {defaulting ? (
              <SetDefaultDialog
                workspaceId={workspaceId}
                theme={defaulting}
                current={data.find((theme) => theme.is_default) ?? null}
                onClose={() => setDefaulting(null)}
                onDone={() => {
                  setDefaulting(null);
                  themes.reload();
                }}
              />
            ) : null}
          </>
        )}
      </ResourceView>
      {adding ? <NewThemeDialog workspaceId={workspaceId} onClose={() => setAdding(false)} /> : null}
      {archiving ? (
        <ArchiveDialog
          workspaceId={workspaceId}
          theme={archiving}
          onClose={() => setArchiving(null)}
          onArchived={() => {
            setArchiving(null);
            themes.reload();
          }}
        />
      ) : null}
    </div>
  );
}

function ThemeCard({
  workspaceId,
  theme,
  canAdmin,
  onSetDefault,
  onArchive,
}: {
  workspaceId: string;
  theme: ThemeListItem;
  canAdmin: boolean;
  onSetDefault: () => void;
  onArchive: () => void;
}) {
  const href = `/w/${workspaceId}/themes/${theme.id}`;
  const published = isPublished(theme);
  return (
    <li className="th-card">
      <Link to={href} tabIndex={-1} aria-hidden="true" className="th-card-thumb">
        <ThemeThumb tokens={readTokens(theme.tokens)} name={theme.name} />
      </Link>
      <div className="th-card-foot">
        <div className="th-card-text">
          <div className="th-card-title">
            <Link to={href} className="strong">
              {theme.name}
            </Link>
            {theme.is_default ? <span className="pill">Default</span> : null}
            {published ? null : <Pill tone="attention">Draft</Pill>}
          </div>
          <div className="faint">
            {published ? `v${theme.current_version} · ${plural(theme.used_by_templates, "template", "templates")}` : "Not published"}
          </div>
        </div>
        <ThemeMenu theme={theme} canAdmin={canAdmin} onSetDefault={onSetDefault} onArchive={onArchive} />
      </div>
    </li>
  );
}

function NewThemeDialog({ workspaceId, onClose }: { workspaceId: string; onClose: () => void }) {
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [taken, setTaken] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (busy || name.trim() === "") return;
    setBusy(true);
    setFailure(null);
    setTaken(false);
    try {
      const created = await unwrap(client.POST("/api/v1/workspaces/{ws}/themes", { params: { path: { ws: workspaceId } }, body: { name: name.trim() } }));
      void navigate(`/w/${workspaceId}/themes/${created.id}`);
    } catch (error) {
      if (error instanceof ApiError && error.status === 409) setTaken(true);
      else setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title="New theme" onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <TextField
          label="Name"
          hint="Up to 100 characters. Unique among the live themes."
          value={name}
          onChange={(event) => setName(event.target.value)}
          maxLength={100}
          autoComplete="off"
          required
          error={taken ? "A theme with that name already exists. Choose another." : undefined}
        />
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || name.trim() === ""}>
            {busy ? "Creating" : "Create theme"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
