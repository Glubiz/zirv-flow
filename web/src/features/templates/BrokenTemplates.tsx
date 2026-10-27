import { Link } from "react-router";

import type { BrokenTemplate } from "../../api";

interface Props {
  workspaceId: string;
  /** What the refusal says to do. */
  detail: string;
  templates: readonly BrokenTemplate[];
  /** How many templates the refusal counts, of which `templates` lists the first. */
  total: number;
}

/** The live templates a snippet or theme version would stop from sending, each linked with its issue codes. */
export function BrokenTemplates({ workspaceId, detail, templates, total }: Props) {
  return (
    <section className="stack-tight" role="alert" aria-label="Templates it would break">
      <p>{detail}</p>
      <ul className="list-plain" aria-label="Templates it would break">
        {templates.map((template) => (
          <li key={template.id} className="row">
            <Link to={`/w/${workspaceId}/templates/${template.id}`}>{template.name}</Link>
            <span className="faint">{template.codes.join(", ")}</span>
          </li>
        ))}
      </ul>
      {total > templates.length ? <p className="muted">and {total - templates.length} more</p> : null}
    </section>
  );
}
