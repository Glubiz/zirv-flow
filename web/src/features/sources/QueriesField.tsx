import { Button, TextAreaField, TextField } from "../../ui";
import {
  MAX_QUERIES,
  MAX_QUERY_NAME_CHARS,
  nameProblem,
  parseQueries,
  serializeQueries,
  sqlProblem,
  type Query,
} from "./queries";

interface QueriesFieldProps {
  label: string;
  hint?: string;
  /** The JSON text of the list, as the form keeps it. */
  value: string;
  onChange: (value: string) => void;
  disabled?: boolean;
}

/**
 * The queries of a database source: a list of rows, each a name and a SELECT. Every query becomes
 * a stream named `query:<name>`. Names and SQL are checked as they are typed, with the same rules
 * the server applies.
 */
export function QueriesField({ label, hint, value, onChange, disabled = false }: QueriesFieldProps) {
  const rows = parseQueries(value);

  if (rows === null) {
    return (
      <fieldset className="stack">
        <legend className="strong">{label}</legend>
        <p className="field-error" role="alert">
          The queries could not be read. Start over to enter them again.
        </p>
        <div className="row">
          <Button onClick={() => onChange("")} disabled={disabled}>
            Start over
          </Button>
        </div>
      </fieldset>
    );
  }

  const update = (index: number, change: Partial<Query>) =>
    onChange(serializeQueries(rows.map((row, at) => (at === index ? { ...row, ...change } : row))));

  return (
    <fieldset className="stack">
      <legend className="strong">{label}</legend>
      {hint ? <p className="field-hint">{hint}</p> : null}
      <p className="field-hint">
        One SELECT or WITH statement per query, with no semicolon at the end. Include the columns you set as the
        primary key and the cursor; no ORDER BY is needed.
      </p>
      {rows.length === 0 ? <p className="muted">No queries</p> : null}
      {rows.map((row, index) => {
        const sqlError = row.sql === "" ? undefined : sqlProblem(row.sql);
        return (
          // The rows have no identity of their own; the list is only ever edited in place.
          <div className="row-card stack" key={index}>
            <TextField
              label={`Query ${index + 1} name`}
              hint={row.name ? `Offered as the stream query:${row.name}` : undefined}
              value={row.name}
              maxLength={MAX_QUERY_NAME_CHARS * 2}
              error={row.name === "" ? undefined : nameProblem(rows, index)}
              disabled={disabled}
              autoComplete="off"
              mono
              onChange={(event) => update(index, { name: event.target.value })}
            />
            <TextAreaField
              label={`Query ${index + 1} SQL`}
              value={row.sql}
              rows={5}
              mono
              spellCheck={false}
              disabled={disabled}
              onChange={(event) => update(index, { sql: event.target.value })}
            />
            {sqlError ? (
              <span className="field-error" role="alert">
                {sqlError}
              </span>
            ) : null}
            <div className="row">
              <Button onClick={() => onChange(serializeQueries(rows.filter((_, at) => at !== index)))} disabled={disabled}>
                {`Remove query ${index + 1}`}
              </Button>
            </div>
          </div>
        );
      })}
      <div className="row">
        <Button
          onClick={() => onChange(serializeQueries([...rows, { name: "", sql: "" }]))}
          disabled={disabled || rows.length >= MAX_QUERIES}
        >
          Add query
        </Button>
        <span className="faint">{`${rows.length} of ${MAX_QUERIES}`}</span>
      </div>
    </fieldset>
  );
}
