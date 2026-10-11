import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session, type Call } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { stepFor, ImportPage } from "./ImportPage";
import type { CsvImport } from "./labels";
import { anImport, LISTS, SAMPLES, TRAITS } from "./testData";

const members = (role: "admin" | "member", userId: string) => [
  { user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" },
];

const REJECTS = {
  rejects: [
    {
      row_number: 4,
      phase: "dry_run",
      errors: [
        { column: "Email", reason: "invalid_identifier" },
        { column: "Plan", reason: "wrong_type" },
      ],
    },
  ],
  next_after: null,
};

interface Server {
  current: CsvImport;
  masked: boolean;
  routes: Parameters<typeof mockApi>[0];
}

/** A small stand-in for the API: the import it serves changes as the page acts on it. */
function server(start: Partial<CsvImport>, options: { masked?: boolean; role?: "admin" | "member"; user?: typeof OWNER; memberId?: string } = {}): Server {
  const user = options.user ?? OWNER;
  const state: Server = { current: anImport(start), masked: options.masked ?? false, routes: {} };
  const base = "/api/v1/workspaces/ws1/imports/imp-1";
  state.routes = {
    "GET /api/v1/session": () => json(session(user)),
    "GET /api/v1/workspaces/ws1/members": () => json(members(options.role ?? "admin", options.memberId ?? user.id)),
    "GET /api/v1/workspaces/ws1/traits": () => json(TRAITS),
    "GET /api/v1/workspaces/ws1/lists": () => json(LISTS),
    [`GET ${base}`]: (request) => {
      const withSamples = new URL(request.url).searchParams.get("samples") === "true";
      return json({
        ...state.current,
        samples: withSamples ? SAMPLES.map((row) => row.map((cell) => (state.masked ? null : cell))) : [],
        samples_masked: state.masked,
      });
    },
    [`PUT ${base}/mapping`]: async (request) => {
      state.current = { ...state.current, status: "mapped", mapping: (await request.json()) as CsvImport["mapping"] };
      return json(state.current);
    },
    [`POST ${base}/dry-run`]: () => {
      state.current = { ...state.current, status: "dry_running", rows_done: 0, rows_applied: 0, rows_rejected: 0 };
      return json(state.current, 202);
    },
    [`POST ${base}/run`]: () => {
      state.current = { ...state.current, status: "running", started_at: "2026-10-01T10:00:00Z", rows_done: 0 };
      return json(state.current, 202);
    },
    [`POST ${base}/cancel`]: () => {
      state.current = { ...state.current, status: "cancelled", finished_at: "2026-10-01T10:05:00Z" };
      return json(state.current);
    },
    [`GET ${base}/rejects`]: () => json(REJECTS),
  };
  return state;
}

function mount(pollMs = 20) {
  return renderRoute("/w/ws1/imports/imp-1", "/w/:ws/imports/:importId", <ImportPage pollMs={pollMs} />);
}

async function openSteps(user: ReturnType<typeof userEvent.setup>, column: string) {
  await user.click(
    screen.getByText((_, element) => element?.tagName === "SUMMARY" && element.textContent.endsWith(`for ${column}`)),
  );
}

function bodyOf(calls: Call[], method: string, suffix: string): unknown {
  return calls.find((call) => call.method === method && call.path.endsWith(suffix))?.body;
}

describe("the step an import is in", () => {
  it("follows its state", () => {
    const step = (change: Partial<CsvImport>) => stepFor(anImport(change));

    expect(step({ status: "uploaded" })).toBe("map");
    expect(step({ status: "mapped" })).toBe("dry-run");
    expect(step({ status: "dry_running" })).toBe("dry-run");
    expect(step({ status: "dry_run_done" })).toBe("dry-run");
    expect(step({ status: "running" })).toBe("run");
    expect(step({ status: "succeeded" })).toBe("run");
    expect(step({ status: "failed", started_at: "2026-10-01T10:00:00Z" })).toBe("run");
    expect(step({ status: "failed" })).toBe("dry-run");
    expect(step({ status: "cancelled" })).toBe("dry-run");
  });
});

describe("the map step", () => {
  it("shows every column with its first values and guesses the email column", async () => {
    mockApi(server({}).routes);
    mount();

    const table = await screen.findByRole("table", { name: "Columns of the file" });
    expect(within(table).getByRole("rowheader", { name: "Email" })).toBeInTheDocument();
    expect(within(table).getByText("ada@example.test · bob@example.test")).toBeInTheDocument();
    expect(screen.getByLabelText("What Email becomes")).toHaveValue("identifier:email");
    expect(screen.getByLabelText("What Plan becomes")).toHaveValue("ignore");
    expect(screen.getByRole("list", { name: "Import steps" }).querySelector("[aria-current=step]")).toHaveTextContent("Map");
  });

  it("offers the fields that are not archived and the lists", async () => {
    mockApi(server({}).routes);
    mount();

    const select = await screen.findByLabelText("What Plan becomes");
    const options = within(select).getAllByRole("option").map((option) => option.textContent);
    expect(options).toContain("plan (string)");
    expect(options).not.toContain("old_plan (string)");
    expect(options).toContain("Newsletter");
    expect(options).toContain("External ID…");
  });

  it("will not save while no column identifies a person, and says why", async () => {
    mockApi(server({}).routes);
    const user = userEvent.setup();
    mount();

    await user.selectOptions(await screen.findByLabelText("What Email becomes"), "ignore");

    expect(screen.getByText("Choose at least one column that identifies a person, such as an email address.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save and dry run" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Save mapping" })).toBeDisabled();
  });

  it("asks for the name of an external ID", async () => {
    mockApi(server({}).routes);
    const user = userEvent.setup();
    mount();

    await user.selectOptions(await screen.findByLabelText("What Plan becomes"), "identifier:external");
    expect(screen.getByText("Finish the choice for Plan.")).toBeInTheDocument();
    await user.type(screen.getByLabelText("Name of the external ID in Plan"), "crm");

    expect(screen.queryByText("Finish the choice for Plan.")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save and dry run" })).toBeEnabled();
  });

  it("saves the mapping with its steps and starts the dry run", async () => {
    const api = server({});
    const calls = mockApi(api.routes);
    const user = userEvent.setup();
    mount();

    await user.selectOptions(await screen.findByLabelText("What Plan becomes"), "trait:plan");
    await user.selectOptions(screen.getByLabelText("What Newsletter becomes"), "list:list-1");
    await openSteps(user, "Email");
    await user.selectOptions(screen.getByLabelText("Add a step to Email"), "constant");
    await user.selectOptions(screen.getByLabelText("Key column"), "Email");
    await user.click(screen.getByRole("button", { name: "Save and dry run" }));

    expect(await screen.findByText(/Checking row 0 of 3/)).toBeInTheDocument();
    const saved = bodyOf(calls, "PUT", "/mapping") as { key_column: string; columns: { column: string }[] };
    expect(saved.key_column).toBe("Email");
    expect(saved.columns.map((column) => column.column)).toEqual(["Email", "Plan", "Newsletter"]);
    expect(saved).toMatchObject({
      columns: [
        {
          column: "Email",
          target: { type: "identifier", kind: "email" },
          transforms: [{ op: "trim" }, { op: "lower" }, { op: "constant", value: "" }],
        },
        { column: "Plan", target: { type: "trait", key: "plan" }, transforms: [] },
        { column: "Newsletter", target: { type: "list", list_id: "list-1" }, transforms: [] },
      ],
    });
    expect(calls.map((call) => `${call.method} ${call.path.split("/imp-1")[1] ?? ""}`)).toContain("POST /dry-run");
  });

  it("keeps the mapping and stays on the step when only saving", async () => {
    const calls = mockApi(server({}).routes);
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Save mapping" }));

    expect(await screen.findByText("The mapping is saved.", { exact: false })).toBeInTheDocument();
    expect(calls.some((call) => call.path.endsWith("/dry-run"))).toBe(false);
    expect(screen.getByRole("button", { name: "Run the dry run" })).toBeInTheDocument();
  });

  it("shows why the server refused the mapping", async () => {
    const api = server({});
    api.routes["PUT /api/v1/workspaces/ws1/imports/imp-1/mapping"] = () =>
      problem(422, "Unprocessable Entity", 'mapping: unknown_trait (column "Plan")', "trace-9");
    mockApi(api.routes);
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Save and dry run" }));

    expect(await screen.findByText('mapping: unknown_trait (column "Plan")')).toBeInTheDocument();
    expect(screen.getByText("trace-9")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save and dry run" })).toBeEnabled();
  });

  it("hides the sample values from someone who may not see values", async () => {
    mockApi(server({}, { masked: true, role: "member", user: MEMBER }).routes);
    mount();

    expect(await screen.findByText("Sample values are hidden for your role. The mapping works the same.")).toBeInTheDocument();
    expect(screen.queryByText(/ada@example.test/)).not.toBeInTheDocument();
    expect(screen.getAllByText("Hidden")).toHaveLength(3);
  });

  it("lets a reader look but not change", async () => {
    mockApi(server({}, { role: "admin", user: MEMBER, memberId: "someone-else" }).routes);
    mount();

    expect(await screen.findByText("You can look at this import but not change it.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Save and dry run" })).not.toBeInTheDocument();
    expect(screen.getByLabelText("What Email becomes")).toBeDisabled();
  });

  it("shows a loading state and a retry when the import cannot be loaded", async () => {
    let attempts = 0;
    const api = server({});
    api.routes["GET /api/v1/workspaces/ws1/imports/imp-1"] = () => {
      attempts += 1;
      return attempts === 1 ? problem(404, "Not Found", "no such resource") : json({ ...anImport({}), samples: [], samples_masked: false });
    };
    mockApi(api.routes);
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("heading", { name: "Customers May" })).toBeInTheDocument();
  });
});

describe("the transform steps", () => {
  it("edits a step's settings and leaves the finished ones in the saved mapping", async () => {
    const calls = mockApi(server({}).routes);
    const user = userEvent.setup();
    mount();

    await user.selectOptions(await screen.findByLabelText("What Plan becomes"), "trait:plan");
    await openSteps(user, "Plan");
    await user.selectOptions(screen.getAllByLabelText("Add a step to Plan")[0]!, "value_map");
    await user.type(screen.getByLabelText("Replacements of step 1 of Plan"), "p=pro");
    await user.selectOptions(screen.getAllByLabelText("Add a step to Plan")[0]!, "split");
    await user.clear(screen.getByLabelText("Separator of step 2 of Plan"));
    await user.type(screen.getByLabelText("Separator of step 2 of Plan"), ";");
    await user.click(screen.getByRole("button", { name: "Remove step 2 of Plan" }));
    await user.click(screen.getByRole("button", { name: "Save mapping" }));

    await screen.findByText("The mapping is saved.", { exact: false });
    expect(bodyOf(calls, "PUT", "/mapping")).toMatchObject({
      columns: [
        { column: "Email" },
        { column: "Plan", transforms: [{ op: "value_map", map: { p: "pro" } }] },
      ],
    });
  });
});

describe("the dry run step", () => {
  it("follows the dry run and then shows the counts and the first rejected rows", async () => {
    const api = server({ status: "dry_running", rows_total: 3, rows_done: 1 });
    mockApi(api.routes);
    mount();

    expect(await screen.findByText(/Checking row 1 of 3/)).toBeInTheDocument();
    expect(screen.getByRole("progressbar", { name: "Dry run progress" })).toHaveAttribute("value", "33");
    api.current = { ...api.current, status: "dry_run_done", rows_done: 3, rows_applied: 2, rows_rejected: 1 };

    expect(await screen.findByText(/3 rows checked: 2 would be imported, 1 would be rejected/, undefined, { timeout: 3000 })).toBeInTheDocument();
    const table = await screen.findByRole("table", { name: "First rejected rows" });
    expect(within(table).getByText("4")).toBeInTheDocument();
    expect(within(table).getByText("the identifier is not valid for its kind")).toBeInTheDocument();
    expect(within(table).getByText("the value does not fit the field's type")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Download all rejected rows" })).toHaveAttribute(
      "href",
      "/api/v1/workspaces/ws1/imports/imp-1/rejects.csv?phase=dry_run",
    );
    expect(screen.getByRole("button", { name: "Start the import" })).toBeEnabled();
  });

  it("warns when so many rows are rejected that a run would stop", async () => {
    mockApi(
      server({ status: "dry_run_done", rows_total: 5000, rows_done: 5000, rows_applied: 4000, rows_rejected: 1000 }).routes,
    );
    mount();

    expect(await screen.findByText(/More than 5% of the rows are rejected/)).toBeInTheDocument();
  });

  it("can be cancelled while it runs", async () => {
    const api = server({ status: "dry_running", rows_total: 100, rows_done: 10 });
    const calls = mockApi(api.routes);
    const user = userEvent.setup();
    mount(10_000);

    await user.click(await screen.findByRole("button", { name: "Cancel the dry run" }));

    expect(await screen.findByText("The dry run was cancelled.")).toBeInTheDocument();
    expect(calls.some((call) => call.method === "POST" && call.path.endsWith("/cancel"))).toBe(true);
  });

  it("starts the import from the dry run and shows its progress", async () => {
    const api = server({ status: "dry_run_done", rows_total: 3, rows_done: 3, rows_applied: 3 });
    const calls = mockApi(api.routes);
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Start the import" }));

    expect(await screen.findByText(/Importing row 0 of 3/)).toBeInTheDocument();
    expect(calls.some((call) => call.method === "POST" && call.path.endsWith("/run"))).toBe(true);
    expect(screen.getByRole("button", { name: "Cancel the import" })).toBeInTheDocument();
  });

  it("goes back to the mapping", async () => {
    mockApi(server({ status: "dry_run_done", rows_total: 3, rows_done: 3, rows_applied: 3 }).routes);
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Back to the mapping" }));

    expect(await screen.findByRole("table", { name: "Columns of the file" })).toBeInTheDocument();
  });

  it("lets a mapped import skip the dry run", async () => {
    const api = server({ status: "mapped" });
    const calls = mockApi(api.routes);
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Skip it and start the import" }));

    expect(await screen.findByText(/Importing row 0 of 3/)).toBeInTheDocument();
    expect(calls.some((call) => call.path.endsWith("/dry-run") && call.method === "POST")).toBe(false);
  });
});

describe("the run step", () => {
  it("follows the counters while it runs and ends on a summary", async () => {
    const api = server({ status: "running", started_at: "2026-10-01T10:00:00Z", rows_total: 4, rows_done: 1, rows_applied: 1 });
    mockApi(api.routes);
    mount();

    expect(await screen.findByText(/Importing row 1 of 4/)).toBeInTheDocument();
    expect(screen.getByRole("progressbar", { name: "Import progress" })).toHaveAttribute("value", "25");
    api.current = { ...api.current, rows_done: 3, rows_applied: 3 };
    await waitFor(() => expect(screen.getByRole("progressbar", { name: "Import progress" })).toHaveAttribute("value", "75"), {
      timeout: 3000,
    });
    api.current = {
      ...api.current,
      status: "succeeded",
      rows_done: 4,
      rows_applied: 3,
      rows_unchanged: 1,
      finished_at: "2026-10-01T10:02:00Z",
    };

    expect(await screen.findByText(/Imported 4 rows/, undefined, { timeout: 3000 })).toBeInTheDocument();
    expect(screen.queryByRole("progressbar")).not.toBeInTheDocument();
    const facts = screen.getByText("Unchanged").closest("div")!;
    expect(within(facts).getByText("1")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Back to imports" })).toHaveAttribute("href", "/w/ws1/imports");
  });

  it("can be cancelled while it runs and says the applied rows stay", async () => {
    const api = server({ status: "running", started_at: "2026-10-01T10:00:00Z", rows_total: 100, rows_done: 40, rows_applied: 40 });
    const calls = mockApi(api.routes);
    const user = userEvent.setup();
    mount(10_000);

    await user.click(await screen.findByRole("button", { name: "Cancel the import" }));

    expect(await screen.findByText(/cancelled after 40 rows/)).toBeInTheDocument();
    expect(screen.getByText(/Rows already applied stay applied/)).toBeInTheDocument();
    expect(calls.filter((call) => call.method === "POST" && call.path.endsWith("/cancel"))).toHaveLength(1);
  });

  it("explains a run that the reject threshold stopped, with its rejected rows", async () => {
    mockApi({
      ...server({
        status: "failed",
        error_code: "reject_threshold",
        started_at: "2026-10-01T10:00:00Z",
        rows_total: 5000,
        rows_done: 1500,
        rows_applied: 1200,
        rows_rejected: 300,
      }).routes,
    });
    mount();

    expect(await screen.findByText("The import failed.")).toBeInTheDocument();
    expect(screen.getByText(/Too many rows were rejected, so the import stopped/)).toBeInTheDocument();
    expect(await screen.findByRole("table", { name: "First rejected rows" })).toBeInTheDocument();
    expect(screen.getByText(/299 more rejected rows are in the download/)).toBeInTheDocument();
  });

  it("gives a reason for each other failure code", async () => {
    mockApi(server({ status: "failed", error_code: "file_missing", started_at: "2026-10-01T10:00:00Z" }).routes);
    mount();

    expect(await screen.findByText(/The uploaded file is no longer stored/)).toBeInTheDocument();
  });

  it("offers a reader no cancel", async () => {
    mockApi(
      server({ status: "running", started_at: "2026-10-01T10:00:00Z", rows_total: 10, rows_done: 2 }, { user: MEMBER, memberId: "someone-else" })
        .routes,
    );
    mount();

    expect(await screen.findByText(/Importing row 2 of 10/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Cancel the import" })).not.toBeInTheDocument();
  });
});
