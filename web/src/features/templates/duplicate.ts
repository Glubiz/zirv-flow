import { ApiError, client, unwrap } from "../../api";
import { copyNames } from "./library";
import { asBody, type Template, type TemplateDetail } from "./model";

/** The channel and category of a stored template, which the create call takes in the same words. */
function kinds(detail: Template): { channel: "email" | "webhook"; category: "marketing" | "transactional" } {
  return {
    channel: detail.channel === "webhook" ? "webhook" : "email",
    category: detail.category === "transactional" ? "transactional" : "marketing",
  };
}

export function readTemplate(workspaceId: string, id: string): Promise<TemplateDetail> {
  return unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}", { params: { path: { ws: workspaceId, id } } }));
}

/** Makes a template named `name` with the draft document of `source`, in its channel and category. */
export function createFrom(workspaceId: string, name: string, source: TemplateDetail): Promise<Template> {
  return unwrap(
    client.POST("/api/v1/workspaces/{ws}/templates", {
      params: { path: { ws: workspaceId } },
      body: { name, ...kinds(source), document: asBody(source.draft.document as Record<string, unknown>) },
    }),
  );
}

/**
 * Copies a template: reads its draft, then creates "Copy of <name>" with that document. A name a live
 * template already has gets " 2", " 3" and so on. Names already known to be taken are skipped; the
 * server's refusal of one is the last word.
 */
export async function duplicateTemplate(workspaceId: string, id: string, known: ReadonlySet<string>): Promise<Template> {
  const source = await readTemplate(workspaceId, id);
  for (const name of copyNames(source.name)) {
    if (known.has(name.toLowerCase())) continue;
    try {
      return await createFrom(workspaceId, name, source);
    } catch (error) {
      if (!(error instanceof ApiError) || error.status !== 409) throw error;
    }
  }
  throw new Error("Every copy name is taken. Rename an earlier copy and try again.");
}
