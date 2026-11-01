import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { AiSettingsPage } from "./AiSettingsPage";

const SECRET = "sk-live-must-never-be-prefilled";

const MODELS = {
  min_context_window_tokens: 32768,
  providers: [
    {
      id: "openai",
      kind: "openai_compatible",
      base_url: "https://api.openai.com/v1",
      models: [
        {
          name: "big",
          context_window_tokens: 128000,
          input_price_per_mtok: 3000000,
          output_price_per_mtok: 15000000,
          meets_minimum_context: true,
        },
        {
          name: "small",
          context_window_tokens: 8192,
          input_price_per_mtok: 100000,
          output_price_per_mtok: 200000,
          meets_minimum_context: false,
        },
      ],
    },
    {
      id: "anthropic",
      kind: "anthropic",
      base_url: "https://api.anthropic.com",
      models: [
        {
          name: "claude",
          context_window_tokens: 200000,
          input_price_per_mtok: 3000000,
          output_price_per_mtok: 15000000,
          meets_minimum_context: true,
        },
      ],
    },
  ],
};

const OFF = {
  enabled: false,
  provider_id: null,
  model: null,
  monthly_budget_micros: 0,
  pii_opt_in: false,
  transcript_days: null,
  provider_key_set: false,
  last_probe: null,
  updated_at: null,
};

const ON = {
  enabled: true,
  provider_id: "openai",
  model: "big",
  monthly_budget_micros: 5_000_000,
  pii_opt_in: true,
  transcript_days: 14,
  provider_key_set: true,
  last_probe: { ok: true, missing: [], at: "2026-10-10T08:00:00Z" },
  updated_at: "2026-10-10T08:00:00Z",
};

const USAGE = {
  month_start: "2026-10-01T00:00:00Z",
  spent_micros: 2_100_000,
  budget_micros: 5_000_000,
  by_feature: [{ feature: "mapping", calls: 3, input_tokens: 3000, output_tokens: 900, cost_micros: 2_100_000 }],
  by_user: [{ user_id: "0190aaaa-0000-7000-8000-000000000001", calls: 3, cost_micros: 2_100_000 }],
  recent: [
    {
      id: "c1",
      user_id: null,
      feature: "mapping",
      provider_id: "openai",
      model: "big",
      input_tokens: 1000,
      output_tokens: 300,
      estimated_cost_micros: 700_000,
      latency_ms: 800,
      outcome: "timeout",
      created_at: "2026-10-10T09:00:00Z",
    },
    {
      id: "c2",
      user_id: null,
      feature: "mapping",
      provider_id: "openai",
      model: "big",
      input_tokens: 0,
      output_tokens: 0,
      estimated_cost_micros: 150_000,
      latency_ms: 0,
      outcome: "pending",
      created_at: "2026-10-10T09:00:01Z",
    },
    {
      id: "c3",
      user_id: null,
      feature: "mapping",
      provider_id: "openai",
      model: "big",
      input_tokens: 0,
      output_tokens: 0,
      estimated_cost_micros: 150_000,
      latency_ms: 0,
      outcome: "abandoned",
      created_at: "2026-10-10T09:00:02Z",
    },
  ],
};

const EMPTY_USAGE = { ...USAGE, spent_micros: 0, by_feature: [], by_user: [], recent: [] };

function admin(overrides: Record<string, Handler> = {}) {
  return {
    "GET /api/v1/session": () => json(session(OWNER, "csrf-ai")),
    "GET /api/v1/workspaces/ws1/members": () =>
      json([{ user_id: OWNER.id, email: OWNER.email, role: "admin", added_at: "2026-10-01T09:00:00Z" }]),
    "GET /api/v1/workspaces/ws1/ai/models": () => json(MODELS),
    "GET /api/v1/workspaces/ws1/ai/settings": () => json(OFF),
    "GET /api/v1/workspaces/ws1/ai/usage": () => json(EMPTY_USAGE),
    ...overrides,
  };
}

function open() {
  return renderRoute("/w/ws1/ai", "/w/:ws/ai", <AiSettingsPage />);
}

describe("AI assistant settings", () => {
  it("says AI is not configured when the deployment lists no providers", async () => {
    mockApi(admin({ "GET /api/v1/workspaces/ws1/ai/models": () => json({ providers: [], min_context_window_tokens: 32768 }) }));
    open();

    expect(await screen.findByText("AI is not configured on this deployment")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Save" })).not.toBeInTheDocument();
  });

  it("shows a loading state while the models load", async () => {
    let release: () => void = () => undefined;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    mockApi(
      admin({
        "GET /api/v1/workspaces/ws1/ai/models": async () => {
          await gate;
          return json(MODELS);
        },
      }),
    );
    open();

    expect(await screen.findByText("Loading the available models")).toBeInTheDocument();
    release();
    expect(await screen.findByRole("button", { name: "Save" })).toBeInTheDocument();
  });

  it("shows an error state with a retry when the models fail", async () => {
    let failing = true;
    mockApi(
      admin({
        "GET /api/v1/workspaces/ws1/ai/models": () =>
          failing ? problem(500, "Internal Server Error", "internal error", "trace-ai") : json(MODELS),
      }),
    );
    const user = userEvent.setup();
    open();

    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText("internal error")).toBeInTheDocument();
    expect(within(alert).getByText("trace-ai")).toBeInTheDocument();

    failing = false;
    await user.click(within(alert).getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("button", { name: "Save" })).toBeInTheDocument();
  });

  it("keeps the page from people who cannot manage the workspace", async () => {
    mockApi(
      admin({
        "GET /api/v1/session": () => json(session(MEMBER)),
        "GET /api/v1/workspaces/ws1/members": () =>
          json([{ user_id: MEMBER.id, email: MEMBER.email, role: "editor", added_at: "2026-10-01T09:00:00Z" }]),
      }),
    );
    open();

    expect(await screen.findByText("You cannot manage the AI assistant here")).toBeInTheDocument();
    expect(screen.queryByLabelText("Provider key")).not.toBeInTheDocument();
  });

  it("never prefills the provider key and only says whether one is set", async () => {
    mockApi(admin({ "GET /api/v1/workspaces/ws1/ai/settings": () => json({ ...ON, provider_key: SECRET }) }));
    open();

    const key = await screen.findByLabelText("Provider key");
    expect(key).toHaveValue("");
    expect(key).toHaveAttribute("type", "password");
    expect(screen.getByText("Key set")).toBeInTheDocument();
    expect(document.body.innerHTML).not.toContain(SECRET);
    expect(screen.getByLabelText("Provider")).toHaveValue("openai");
    expect(screen.getByLabelText("Model")).toHaveValue("big");
    expect(screen.getByLabelText("Monthly budget")).toHaveValue(5);
    expect(screen.getByText(/The last check passed/)).toBeInTheDocument();
  });

  it("shows the context window of the chosen model and warns when it is too small", async () => {
    mockApi(admin());
    const user = userEvent.setup();
    open();

    await user.selectOptions(await screen.findByLabelText("Provider"), "openai");
    expect(screen.getByLabelText("Model")).toHaveValue("big");
    expect(screen.getByText(/Context window: 128,000 tokens\./)).toBeInTheDocument();

    await user.selectOptions(screen.getByLabelText("Model"), "small");
    expect(screen.getByText(/Context window: 8,192 tokens\..*cannot be turned on/)).toBeInTheDocument();
  });

  it("will not turn the assistant on without a model or a key", async () => {
    mockApi(admin());
    const user = userEvent.setup();
    open();

    const save = await screen.findByRole("button", { name: "Save" });
    await user.click(screen.getByRole("switch", { name: "Turn on for this workspace" }));
    expect(save).toBeDisabled();

    await user.selectOptions(screen.getByLabelText("Provider"), "anthropic");
    expect(screen.getByText("A key is needed to turn the assistant on.")).toBeInTheDocument();
    expect(save).toBeDisabled();

    await user.type(screen.getByLabelText("Provider key"), "sk-typed");
    expect(save).toBeEnabled();
  });

  it("has one Save, in the header, that saves every section at once and is off while nothing changed", async () => {
    let saved = false;
    const calls = mockApi(
      admin({
        "GET /api/v1/workspaces/ws1/ai/settings": () =>
          json(saved ? { ...ON, provider_id: "anthropic", model: "claude", transcript_days: 7, pii_opt_in: false, monthly_budget_micros: 2_500_000 } : OFF),
        "PUT /api/v1/workspaces/ws1/ai/settings": () => {
          saved = true;
          return json(ON);
        },
      }),
    );
    const user = userEvent.setup();
    open();

    expect(await screen.findByRole("button", { name: "Save" })).toBeDisabled();
    await user.click(screen.getByRole("switch", { name: "Turn on for this workspace" }));
    await user.selectOptions(screen.getByLabelText("Provider"), "anthropic");
    await user.type(screen.getByLabelText("Provider key"), "sk-typed-key");
    await user.clear(screen.getByLabelText("Monthly budget"));
    await user.type(screen.getByLabelText("Monthly budget"), "2.5");
    await user.click(screen.getByLabelText(/Keep transcripts/));
    expect(screen.getByLabelText("Days to keep transcripts")).toHaveValue(7);
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true));
    expect(calls.filter((call) => call.method === "PUT")).toHaveLength(1);
    expect(screen.getAllByRole("button", { name: /^Save/ })).toHaveLength(1);
    const put = calls.find((call) => call.method === "PUT");
    expect(put?.body).toEqual({
      enabled: true,
      provider_id: "anthropic",
      model: "claude",
      monthly_budget_micros: 2_500_000,
      pii_opt_in: false,
      transcript_days: 7,
      provider_key: "sk-typed-key",
    });
    expect(put?.headers.get("x-csrf-token")).toBe("csrf-ai");
    await waitFor(() => expect(screen.getByText("Key set")).toBeInTheDocument());
    expect(screen.getByLabelText("Provider key")).toHaveValue("");
    expect(document.body.innerHTML).not.toContain("sk-typed-key");
  });

  it("sends no key when the field is left empty and turns transcripts off with null", async () => {
    const calls = mockApi(
      admin({
        "GET /api/v1/workspaces/ws1/ai/settings": () => json(ON),
        "PUT /api/v1/workspaces/ws1/ai/settings": () => json(ON),
      }),
    );
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByLabelText(/Keep transcripts/));
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true));
    const body = calls.find((call) => call.method === "PUT")?.body as Record<string, unknown>;
    expect(body["provider_key"]).toBeNull();
    expect(body["transcript_days"]).toBeNull();
    expect(body["pii_opt_in"]).toBe(true);
  });

  it("validates the transcript window and the budget before saving", async () => {
    mockApi(admin({ "GET /api/v1/workspaces/ws1/ai/settings": () => json(ON) }));
    const user = userEvent.setup();
    open();

    const days = await screen.findByLabelText("Days to keep transcripts");
    expect(days).toHaveValue(14);
    await user.clear(days);
    await user.type(days, "45");
    expect(screen.getByText("Enter a whole number from 1 to 30.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
    await user.clear(days);
    await user.type(days, "30");
    expect(screen.getByRole("button", { name: "Save" })).toBeEnabled();

    await user.clear(screen.getByLabelText("Monthly budget"));
    expect(screen.getByText("Enter an amount of 0 or more.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
  });

  it("lists each capability the probe found missing, with its reason", async () => {
    mockApi(
      admin({
        "PUT /api/v1/workspaces/ws1/ai/settings": () =>
          new Response(
            JSON.stringify({
              type: "about:blank",
              title: "Unprocessable Entity",
              status: 422,
              detail: "the model lacks capabilities the assistant requires",
              code: "ai_capability_missing",
              trace_id: "trace-probe",
              errors: [
                { index: 0, field: "tool_calls", reason: "the model did not call the probe tool" },
                { index: 0, field: "usage", reason: "the model reported zero token usage" },
              ],
            }),
            { status: 422, headers: { "content-type": "application/problem+json" } },
          ),
      }),
    );
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole("switch", { name: "Turn on for this workspace" }));
    await user.selectOptions(screen.getByLabelText("Provider"), "openai");
    await user.type(screen.getByLabelText("Provider key"), "sk-x");
    await user.click(screen.getByRole("button", { name: "Save" }));

    const list = await screen.findByRole("list", { name: "Missing capabilities" });
    const items = within(list).getAllByRole("listitem");
    expect(items).toHaveLength(2);
    expect(items[0]).toHaveTextContent("Tool calls: the model did not call the probe tool");
    expect(items[1]).toHaveTextContent("Token usage: the model reported zero token usage");
    expect(screen.getByText("the model lacks capabilities the assistant requires")).toBeInTheDocument();
    expect(screen.getByText("trace-probe")).toBeInTheDocument();
  });

  it("shows another failure as a plain error without a capability list", async () => {
    mockApi(
      admin({
        "PUT /api/v1/workspaces/ws1/ai/settings": () =>
          problem(502, "Bad Gateway", "egress denied: 10.0.0.5 is a private address"),
      }),
    );
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole("switch", { name: "Turn on for this workspace" }));
    await user.selectOptions(screen.getByLabelText("Provider"), "openai");
    await user.type(screen.getByLabelText("Provider key"), "sk-x");
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(await screen.findByText("egress denied: 10.0.0.5 is a private address")).toBeInTheDocument();
    expect(screen.queryByRole("list", { name: "Missing capabilities" })).not.toBeInTheDocument();
  });

  it("shows usage as a slim feature, calls and cost table with the numbers aligned right", async () => {
    mockApi(
      admin({
        "GET /api/v1/workspaces/ws1/ai/settings": () => json(ON),
        "GET /api/v1/workspaces/ws1/ai/usage": () => json(USAGE),
      }),
    );
    open();

    const table = within(await screen.findByRole("table", { name: "Spend by feature" }));
    expect(table.getAllByRole("columnheader").map((cell) => cell.textContent)).toEqual(["Feature", "Calls", "Cost"]);
    expect(table.getByText("mapping")).toBeInTheDocument();
    expect(table.getByText("3")).toHaveClass("num", "mono");
    expect(table.getByText("2.10")).toHaveClass("num", "mono");
    expect(screen.getByRole("progressbar", { name: "42 percent of the monthly budget used" })).toBeInTheDocument();
    expect(screen.queryByRole("table", { name: "Recent calls" })).not.toBeInTheDocument();
    expect(screen.queryByRole("table", { name: "Spend by user" })).not.toBeInTheDocument();
  });

  it("opens every call in a sheet, with spend by user and the recent calls without content", async () => {
    mockApi(
      admin({
        "GET /api/v1/workspaces/ws1/ai/settings": () => json(ON),
        "GET /api/v1/workspaces/ws1/ai/usage": () => json(USAGE),
      }),
    );
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole("button", { name: "Every call" }));

    const sheet = within(await screen.findByRole("dialog", { name: "Every call" }));
    const byUser = within(sheet.getByRole("table", { name: "Spend by user" }));
    expect(byUser.getByText("0190…0001")).toBeInTheDocument();
    const recent = within(sheet.getByRole("table", { name: "Recent calls" }));
    expect(recent.getByText("timeout")).toBeInTheDocument();
    expect(recent.getByText("pending")).toBeInTheDocument();
    expect(recent.getByText("abandoned")).toBeInTheDocument();
    expect(recent.getByText("1000 / 300")).toBeInTheDocument();
    expect(recent.getByText("0.70")).toBeInTheDocument();

    await user.click(sheet.getByRole("button", { name: "Close" }));
    expect(screen.queryByRole("dialog", { name: "Every call" })).not.toBeInTheDocument();
  });

  it("says there are no calls yet when the month is empty", async () => {
    mockApi(admin());
    open();

    expect(await screen.findByText("No calls this month")).toBeInTheDocument();
  });
});
