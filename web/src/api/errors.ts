/** One invalid field of a request, by position, field name and reason code; never a value. */
export interface FieldError {
  index: number;
  field: string;
  reason: string;
}

/** One problem with a segment definition: a JSON pointer into it and a fixed code, never a value. */
export interface DefinitionIssue {
  pointer: string;
  code: string;
}

export interface ProblemDetails {
  title: string;
  detail: string;
  status: number;
  traceId: string | null;
  requestId: string | null;
  /** A stable code for failures a client acts on, such as `search_timeout`. */
  code?: string | null;
  /** The invalid fields, when the failure lists them. */
  errors?: FieldError[];
  /** What is wrong with a segment definition (`segment_invalid`). */
  issues?: DefinitionIssue[];
  /** What is wrong with a flow's draft (`flow_invalid`). */
  flowIssues?: FlowProblemIssue[];
  /** What is wrong with a snippet's content (`snippet_invalid`). */
  templateIssues?: ProblemTemplateIssue[];
  /** The templates that hold a snippet (`snippet_in_use`). */
  snippetTemplates?: NamedRef[];
  /** The themes that use a snippet (`snippet_in_use`). */
  snippetThemes?: NamedRef[];
  /** The active flows that send a template (`template_in_use`), by id and name. */
  flows?: NamedRef[];
}

/** A thing a problem names, by id and name. */
export interface NamedRef {
  id: string;
  name: string;
}

/** One issue of a template document as a problem lists it. */
export interface ProblemTemplateIssue {
  code: string;
  severity: string;
  at: string;
  field?: string | null;
  message?: string | null;
}

/** One issue of a flow as a problem lists it: a fixed code and the step, connection or pointer it concerns. */
export interface FlowProblemIssue {
  code: string;
  node?: string;
  edge?: string;
  at?: string;
}

export class ApiError extends Error {
  readonly status: number;
  readonly title: string;
  readonly detail: string;
  readonly traceId: string | null;
  readonly requestId: string | null;
  readonly code: string | null;
  readonly errors: FieldError[];
  readonly issues: DefinitionIssue[];
  readonly flowIssues: FlowProblemIssue[];
  readonly templateIssues: ProblemTemplateIssue[];
  readonly snippetTemplates: NamedRef[];
  readonly snippetThemes: NamedRef[];
  readonly flows: NamedRef[];

  constructor(problem: ProblemDetails) {
    super(problem.detail || problem.title);
    this.name = "ApiError";
    this.status = problem.status;
    this.title = problem.title;
    this.detail = problem.detail;
    this.traceId = problem.traceId;
    this.requestId = problem.requestId;
    this.code = problem.code ?? null;
    this.errors = problem.errors ?? [];
    this.issues = problem.issues ?? [];
    this.flowIssues = problem.flowIssues ?? [];
    this.templateIssues = problem.templateIssues ?? [];
    this.snippetTemplates = problem.snippetTemplates ?? [];
    this.snippetThemes = problem.snippetThemes ?? [];
    this.flows = problem.flows ?? [];
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

export function problemFrom(body: unknown, response: Response): ApiError {
  if (isRecord(body)) {
    return new ApiError({
      title: text(body["title"]) ?? (response.statusText || "Request failed"),
      detail: text(body["detail"]) ?? "",
      status: typeof body["status"] === "number" ? body["status"] : response.status,
      traceId: text(body["trace_id"]),
      requestId: text(body["request_id"]),
      code: text(body["code"]),
      errors: fieldErrors(body["errors"]),
      issues: definitionIssues(body["issues"]),
      flowIssues: flowProblemIssues(body["flow_issues"]),
      templateIssues: templateIssues(body["template_issues"]),
      snippetTemplates: namedRefs(body["snippet_templates"]),
      snippetThemes: namedRefs(body["snippet_themes"]),
      flows: flowRefs(body["flows"]),
    });
  }
  return new ApiError({
    title: response.statusText || "Request failed",
    detail: typeof body === "string" ? body : "",
    status: response.status,
    traceId: null,
    requestId: null,
  });
}

export interface ErrorSummary {
  title: string;
  detail: string;
  traceId: string | null;
}

export function describeError(error: unknown): ErrorSummary {
  if (error instanceof ApiError) {
    return { title: error.title, detail: error.detail, traceId: error.traceId };
  }
  return {
    title: "Could not reach the server",
    detail: error instanceof Error ? error.message : "Check your connection and try again.",
    traceId: null,
  };
}

function fieldErrors(value: unknown): FieldError[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((entry: unknown) => {
    if (!isRecord(entry)) return [];
    const { index, field, reason } = entry;
    return typeof index === "number" && typeof field === "string" && typeof reason === "string"
      ? [{ index, field, reason }]
      : [];
  });
}

function definitionIssues(value: unknown): DefinitionIssue[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((entry: unknown) => {
    if (!isRecord(entry)) return [];
    const { pointer, code } = entry;
    return typeof pointer === "string" && typeof code === "string" ? [{ pointer, code }] : [];
  });
}

function flowProblemIssues(value: unknown): FlowProblemIssue[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((entry: unknown) => {
    if (!isRecord(entry) || typeof entry["code"] !== "string") return [];
    const node = text(entry["node"]);
    const edge = text(entry["edge"]);
    const at = text(entry["at"]);
    return [{ code: entry["code"], ...(node ? { node } : {}), ...(edge ? { edge } : {}), ...(at ? { at } : {}) }];
  });
}

function namedRefs(value: unknown): NamedRef[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((entry: unknown) => {
    if (!isRecord(entry)) return [];
    const { id, name } = entry;
    return typeof id === "string" && typeof name === "string" ? [{ id, name }] : [];
  });
}

function flowRefs(value: unknown): NamedRef[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((entry: unknown) => {
    if (!isRecord(entry)) return [];
    const { flow_id: id, flow_name: name } = entry;
    return typeof id === "string" && typeof name === "string" ? [{ id, name }] : [];
  });
}

function templateIssues(value: unknown): ProblemTemplateIssue[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((entry: unknown) => {
    if (!isRecord(entry)) return [];
    const { code, severity, at } = entry;
    if (typeof code !== "string" || typeof severity !== "string" || typeof at !== "string") return [];
    return [{ code, severity, at, field: text(entry["field"]), message: text(entry["message"]) }];
  });
}
