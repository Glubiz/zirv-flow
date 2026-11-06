import { describeError, type components } from "../../api";
import { Button, ErrorState, LoadingState, SelectField } from "../../ui";
import { entryLabel, FIXED_ENTRIES, useWorkspaceSources } from "./sourceOrder";

type SourceBody = components["schemas"]["SourceBody"];

interface SourceOrderListProps {
  workspaceId: string;
  value: string[];
  onChange: (next: string[]) => void;
}

export function SourceOrderList({ workspaceId, value, onChange }: SourceOrderListProps) {
  const sources = useWorkspaceSources(workspaceId);

  if (sources.status === "loading") return <LoadingState label="Loading sources" />;
  if (sources.status === "error") {
    const failure = describeError(sources.error);
    return <ErrorState title="Sources did not load" detail={failure.detail} onRetry={sources.reload} />;
  }

  const known: SourceBody[] = sources.data;
  const choices = [
    ...known.map((source) => ({ value: source.id, label: source.name })),
    ...FIXED_ENTRIES,
  ].filter((choice) => !value.includes(choice.value));

  function move(index: number, by: -1 | 1) {
    const next = [...value];
    const moved = next.splice(index, 1)[0];
    if (moved === undefined) return;
    next.splice(index + by, 0, moved);
    onChange(next);
  }

  return (
    <div className="stack">
      <span className="field-label" id="source-order-label">
        Source order
      </span>
      {value.length > 0 ? (
        <ol className="stack" aria-labelledby="source-order-label">
          {value.map((entry, index) => {
            const label = entryLabel(entry, known);
            return (
              <li key={entry} className="row row-card">
                <span className="row-grow">{label}</span>
                <Button aria-label={`Move ${label} up`} disabled={index === 0} onClick={() => move(index, -1)}>
                  Up
                </Button>
                <Button
                  aria-label={`Move ${label} down`}
                  disabled={index === value.length - 1}
                  onClick={() => move(index, 1)}
                >
                  Down
                </Button>
                <Button aria-label={`Remove ${label}`} onClick={() => onChange(value.filter((_, at) => at !== index))}>
                  Remove
                </Button>
              </li>
            );
          })}
        </ol>
      ) : null}
      {choices.length > 0 ? (
        <SelectField
          label="Add a source"
          hint="Sources, highest first. Others follow by newest."
          value=""
          onChange={(event) => {
            if (event.target.value !== "") onChange([...value, event.target.value]);
          }}
        >
          <option value="">Choose a source</option>
          {choices.map((choice) => (
            <option key={choice.value} value={choice.value}>
              {choice.label}
            </option>
          ))}
        </SelectField>
      ) : (
        <p className="muted">Sources, highest first. Others follow by newest.</p>
      )}
    </div>
  );
}
