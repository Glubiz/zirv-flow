export interface ProblemDetails {
  title: string;
  detail: string;
  status: number;
  traceId: string | null;
  requestId: string | null;
  /** A stable code for failures a client acts on, such as `search_timeout`. */
  code?: string | null;
}

export class ApiError extends Error {
  readonly status: number;
  readonly title: string;
  readonly detail: string;
  readonly traceId: string | null;
  readonly requestId: string | null;
  readonly code: string | null;

  constructor(problem: ProblemDetails) {
    super(problem.detail || problem.title);
    this.name = "ApiError";
    this.status = problem.status;
    this.title = problem.title;
    this.detail = problem.detail;
    this.traceId = problem.traceId;
    this.requestId = problem.requestId;
    this.code = problem.code ?? null;
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
