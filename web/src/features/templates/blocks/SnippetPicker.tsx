import { Link } from "react-router";

import { Button, ResourceView, Sheet } from "../../../ui";
import { usePublishedSnippets, type SnippetOption } from "../../snippets";

interface Props {
  workspaceId: string;
  onPick: (snippet: SnippetOption) => void;
  onClose: () => void;
}

/** The published snippets to put in the email; each is chosen by name. */
export function SnippetPicker({ workspaceId, onPick, onClose }: Props) {
  const snippets = usePublishedSnippets(workspaceId);
  return (
    <Sheet title="Add a snippet" onClose={onClose}>
      <ResourceView
        resource={snippets}
        loadingLabel="Loading snippets"
        isEmpty={(data) => data.length === 0}
        empty={
          <p className="muted">
            No snippet is published yet. <Link to={`/w/${workspaceId}/snippets`}>Open snippets</Link>
          </p>
        }
      >
        {(data) => (
          <ul className="list-plain" aria-label="Published snippets">
            {data.map((snippet) => (
              <li key={snippet.id}>
                <Button aria-label={`Add snippet ${snippet.name}`} onClick={() => onPick(snippet)}>
                  {snippet.name} <span className="faint">v{snippet.version}</span>
                </Button>
              </li>
            ))}
          </ul>
        )}
      </ResourceView>
    </Sheet>
  );
}
