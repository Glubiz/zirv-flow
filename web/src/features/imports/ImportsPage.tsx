import { useState } from "react";
import { Link, useParams } from "react-router";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, EmptyState, formatDateTime, InlineError, PageHeader, Panel, Pill, ResourceView } from "../../ui";
import { useImportAccess } from "./access";
import { type CsvImport, num, plural, rejectsUrl, statusLabel, statusTone } from "./labels";

export function ImportsPage() {
  const { ws = "" } = useParams();
  return <Imports key={ws} workspaceId={ws} />;
}

function NewImportLink({ workspaceId }: { workspaceId: string }) {
  return (
    <Link className="btn btn-primary" to={`/w/${workspaceId}/imports/new`}>
      New import
    </Link>
  );
}

function Imports({ workspaceId }: { workspaceId: string }) {
  const { canWrite } = useImportAccess(workspaceId);
  const first = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/imports", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [older, setOlder] = useState<CsvImport[]>([]);
  const [cursor, setCursor] = useState<string | null | undefined>(undefined);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function showOlder(before: string) {
    setLoadingOlder(true);
    setFailure(null);
    try {
      const page = await unwrap(
        client.GET("/api/v1/workspaces/{ws}/imports", {
          params: { path: { ws: workspaceId }, query: { cursor: before } },
        }),
      );
      setOlder((current) => [...current, ...page.imports]);
      setCursor(page.next_cursor ?? null);
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setLoadingOlder(false);
    }
  }

  return (
    <div className="page">
      <PageHeader
        title="Imports"
        lede="Load people from a CSV file: upload it, map its columns, check the mapping with a dry run, then run it. Running the same file again changes nothing."
        actions={canWrite ? <NewImportLink workspaceId={workspaceId} /> : null}
      />
      <Panel flush>
        <ResourceView
          resource={first}
          loadingLabel="Loading imports"
          isEmpty={(data) => data.imports.length === 0}
          empty={
            <EmptyState title="No imports yet" action={canWrite ? <NewImportLink workspaceId={workspaceId} /> : null}>
              Upload a CSV file of people, such as an export from another system, to bring them in with their fields and lists.
            </EmptyState>
          }
        >
          {(data) => {
            const imports = [...data.imports, ...older];
            const next = cursor === undefined ? data.next_cursor : cursor;
            return (
              <>
                <div className="table-scroll">
                  <table className="table">
                    <caption>Imports of this workspace</caption>
                    <thead>
                      <tr>
                        <th scope="col">Import</th>
                        <th scope="col">Status</th>
                        <th scope="col" className="num">
                          Rows
                        </th>
                        <th scope="col" className="num">
                          Applied
                        </th>
                        <th scope="col" className="num">
                          Unchanged
                        </th>
                        <th scope="col" className="num">
                          Rejected
                        </th>
                        <th scope="col">Uploaded</th>
                        <th scope="col">
                          <span className="visually-hidden">Rejected rows</span>
                        </th>
                      </tr>
                    </thead>
                    <tbody>
                      {imports.map((item) => (
                        <tr key={item.id}>
                          <td className="strong">
                            <Link to={`/w/${workspaceId}/imports/${item.id}`}>{item.name}</Link>
                            <div className="faint">{item.file_name}</div>
                          </td>
                          <td>
                            <Pill tone={statusTone(item.status)}>{statusLabel(item.status)}</Pill>
                          </td>
                          <td className="num">
                            {num(item.rows_done)} / {num(item.rows_total)}
                          </td>
                          <td className="num">{num(item.rows_applied)}</td>
                          <td className="num">{num(item.rows_unchanged)}</td>
                          <td className="num">{num(item.rows_rejected)}</td>
                          <td>{formatDateTime(item.created_at)}</td>
                          <td className="num">
                            {item.rows_rejected > 0 ? (
                              <a href={rejectsUrl(workspaceId, item.id)} download aria-label={`Download rejects of ${item.name}`}>
                                Download rejects
                              </a>
                            ) : null}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
                {failure ? <InlineError {...failure} /> : null}
                {next ? (
                  <div className="panel-footer">
                    <Button onClick={() => void showOlder(next)} disabled={loadingOlder}>
                      {loadingOlder ? "Loading" : "Show older imports"}
                    </Button>
                  </div>
                ) : (
                  <p className="panel-footer faint">{plural(imports.length, "import", "imports")} in all.</p>
                )}
              </>
            );
          }}
        </ResourceView>
      </Panel>
      {!canWrite && first.status === "ready" ? (
        <p className="muted">You can look at imports but not start one.</p>
      ) : null}
    </div>
  );
}
