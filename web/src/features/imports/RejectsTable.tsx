import { client, unwrap, useResource } from "../../api";
import { EmptyState, ResourceView } from "../../ui";
import { num, reasonText } from "./labels";

const SHOWN = 20;

interface RejectsTableProps {
  workspaceId: string;
  importId: string;
  phase: "dry_run" | "run";
  /** How many rows were rejected, so the table can say how many it leaves out. */
  total: number;
}

/** The first rejected rows with what was wrong, as codes in words. Never a cell of the file. */
export function RejectsTable({ workspaceId, importId, phase, total }: RejectsTableProps) {
  const rejects = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/imports/{id}/rejects", {
          params: { path: { ws: workspaceId, id: importId }, query: { phase, limit: SHOWN } },
        }),
      ),
    [workspaceId, importId, phase, total],
  );
  return (
    <ResourceView
      resource={rejects}
      loadingLabel="Loading rejected rows"
      isEmpty={(data) => data.rejects.length === 0}
      empty={<EmptyState title="No rejected rows">Every row passed the mapping.</EmptyState>}
    >
      {(data) => (
        <div className="stack-tight">
          <div className="table-scroll">
            <table className="table">
              <caption>First rejected rows</caption>
              <thead>
                <tr>
                  <th scope="col" className="num">
                    Row
                  </th>
                  <th scope="col">What is wrong</th>
                </tr>
              </thead>
              <tbody>
                {data.rejects.map((reject) => (
                  <tr key={reject.row_number}>
                    <td className="num">{reject.row_number}</td>
                    <td>
                      <ul className="list-plain">
                        {reject.errors.map((error, index) => (
                          <li key={`${error.column}-${error.reason}-${index}`}>
                            {error.column ? <span className="strong">{error.column}: </span> : null}
                            {reasonText(error.reason)}
                          </li>
                        ))}
                      </ul>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {total > data.rejects.length ? (
            <p className="faint">
              {num((total - data.rejects.length))} more rejected rows are in the download. Row 1 is the header.
            </p>
          ) : null}
        </div>
      )}
    </ResourceView>
  );
}
