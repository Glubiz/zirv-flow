import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { SendingSettingsPage } from "./SendingSettingsPage";
import { checkPattern, checkUtm, exampleLink, parseDomains } from "./utm";

const WS = "/api/v1/workspaces/ws1";

const UTM_OFF = { enabled: false, source: "{workspace}", medium: "email", campaign: "{flow}", content: "{template}", domains: [] as string[] };

function workspace(utm: typeof UTM_OFF) {
  return { id: "ws1", name: "Spring campaigns", slug: "spring", timezone: "UTC", utm, created_at: "2026-10-01T09:00:00Z" };
}

function routes(role: "admin" | "member", utm: typeof UTM_OFF, extra: Record<string, Handler> = {}): Record<string, Handler> {
  const who = role === "admin" ? OWNER : MEMBER;
  return {
    "GET /api/v1/session": () => json(session(who)),
    "GET /api/v1/workspaces/ws1/members": () => json([{ user_id: who.id, email: who.email, role, added_at: "2026-10-01T09:00:00Z" }]),
    [`GET ${WS}`]: () => json(workspace(utm)),
    [`GET ${WS}/sending/settings`]: () => json({ frequency_caps: [], quiet_hours: null, revision: 1 }),
    [`GET ${WS}/sending-domains`]: () =>
      json([
        { id: "d1", domain: "news.example.com", status: "verified", records: [], selector: "s", created_at: "2026-10-01T09:00:00Z" },
        { id: "d2", domain: "pending.example.com", status: "unverified", records: [], selector: "s", created_at: "2026-10-01T09:00:00Z" },
      ]),
    [`GET ${WS}/test-send-allowlist`]: () => json({ entries: [] }),
    [`GET ${WS}/test-send-allowlist/options`]: () => json([]),
    ...extra,
  };
}

function mount() {
  return renderRoute("/w/ws1/settings/sending", "/w/:ws/settings/sending", <SendingSettingsPage />);
}

describe("UTM parameters section", () => {
  it("shows the stored setting, off by default, with the live example link", async () => {
    mockApi(routes("admin", UTM_OFF));
    mount();

    const form = await screen.findByRole("form", { name: "UTM parameters" });
    expect(within(form).getByRole("checkbox", { name: "Add UTM parameters to email links" })).not.toBeChecked();
    expect(within(form).getByLabelText("Source pattern")).toHaveValue("{workspace}");
    expect(within(form).getByLabelText("Medium pattern")).toHaveValue("email");
    expect(within(form).getByLabelText("Campaign pattern")).toHaveValue("{flow}");
    expect(within(form).getByLabelText("Content pattern")).toHaveValue("{template}");
    expect(within(form).getByLabelText("Example link")).toHaveTextContent(
      "https://example.com/offer?utm_source=spring&utm_medium=email&utm_campaign=welcome-series&utm_content=welcome-email",
    );
    expect(within(form).getByRole("button", { name: "Save UTM parameters" })).toBeDisabled();
  });

  it("updates the example link as the patterns and domains change", async () => {
    mockApi(routes("admin", UTM_OFF));
    const user = userEvent.setup();
    mount();

    const form = await screen.findByRole("form", { name: "UTM parameters" });
    const campaign = within(form).getByLabelText("Campaign pattern");
    await user.clear(campaign);
    await user.click(campaign);
    await user.paste("spring-{workspace}-{flow_version}");
    await user.type(within(form).getByLabelText("Link domains"), "Shop.Example.com");

    expect(within(form).getByLabelText("Example link")).toHaveTextContent(
      "https://shop.example.com/offer?utm_source=spring&utm_medium=email&utm_campaign=spring-spring-3&utm_content=welcome-email",
    );
  });

  it("refuses an unknown placeholder and enabling with no domain, inline, and saves nothing", async () => {
    const calls = mockApi(routes("admin", UTM_OFF));
    const user = userEvent.setup();
    mount();

    const form = await screen.findByRole("form", { name: "UTM parameters" });
    await user.click(within(form).getByRole("checkbox", { name: "Add UTM parameters to email links" }));
    const source = within(form).getByLabelText("Source pattern");
    await user.clear(source);
    await user.click(source);
    await user.paste("{person.email}");
    await user.click(within(form).getByRole("button", { name: "Save UTM parameters" }));

    expect(await within(form).findByText(/Unknown placeholder \{person\.email\}/)).toBeInTheDocument();
    expect(within(form).getByText("List at least one domain before turning UTM parameters on.")).toBeInTheDocument();
    expect(within(form).getByLabelText("Source pattern")).toBeInvalid();
    expect(within(form).getByText("Fix the patterns to see an example link.")).toBeInTheDocument();
    expect(calls.some((call) => call.method === "PATCH")).toBe(false);
  });

  it("suggests the verified sending domains and saves the whole setting", async () => {
    let stored = UTM_OFF;
    const calls = mockApi(
      routes("admin", UTM_OFF, {
        [`GET ${WS}`]: () => json(workspace(stored)),
        [`PATCH ${WS}`]: async (request) => {
          stored = ((await request.json()) as { utm: typeof UTM_OFF }).utm;
          return json(workspace(stored));
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    const form = await screen.findByRole("form", { name: "UTM parameters" });
    const suggestions = await within(form).findByRole("group", { name: "Verified sending domains" });
    expect(within(suggestions).queryByRole("button", { name: "Add pending.example.com" })).not.toBeInTheDocument();
    await user.click(within(suggestions).getByRole("button", { name: "Add news.example.com" }));
    await user.click(within(form).getByRole("checkbox", { name: "Add UTM parameters to email links" }));
    await user.click(within(form).getByRole("button", { name: "Save UTM parameters" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PATCH")).toBe(true));
    expect(calls.find((call) => call.method === "PATCH")?.body).toEqual({
      utm: { ...UTM_OFF, enabled: true, domains: ["news.example.com"] },
    });
    expect(await screen.findByText(/Links to the listed domains are tagged/)).toBeInTheDocument();
  });

  it("shows the refusal of the server", async () => {
    mockApi(
      routes(
        "admin",
        { ...UTM_OFF, domains: ["example.com"] },
        { [`PATCH ${WS}`]: () => problem(422, "Unprocessable", "utm.domains: each entry must be a domain name") },
      ),
    );
    const user = userEvent.setup();
    mount();

    const form = await screen.findByRole("form", { name: "UTM parameters" });
    await user.click(within(form).getByRole("checkbox", { name: "Add UTM parameters to email links" }));
    await user.click(within(form).getByRole("button", { name: "Save UTM parameters" }));

    expect(await within(form).findByText(/each entry must be a domain name/)).toBeInTheDocument();
  });

  it("is read only for a member", async () => {
    mockApi(routes("member", { ...UTM_OFF, enabled: true, domains: ["example.com"] }));
    mount();

    const form = await screen.findByRole("form", { name: "UTM parameters" });
    expect(within(form).getByRole("checkbox", { name: "Add UTM parameters to email links" })).toBeDisabled();
    expect(within(form).getByLabelText("Source pattern")).toBeDisabled();
    expect(within(form).getByLabelText("Link domains")).toBeDisabled();
    expect(within(form).queryByRole("button", { name: "Save UTM parameters" })).not.toBeInTheDocument();
    expect(within(form).getByText(/UTM parameters can be changed by/)).toBeInTheDocument();
  });

  it("shows a refused pattern as soon as it is typed, before any save", async () => {
    mockApi(routes("admin", UTM_OFF));
    const user = userEvent.setup();
    mount();

    const form = await screen.findByRole("form", { name: "UTM parameters" });
    const source = within(form).getByLabelText("Source pattern");
    await user.clear(source);
    await user.click(source);
    await user.paste("{nope}");

    expect(within(form).getByText(/Unknown placeholder {nope}/)).toBeInTheDocument();
    expect(source).toBeInvalid();
  });

  it("shows an error state with a retry when the verified domains cannot be loaded", async () => {
    mockApi(routes("admin", UTM_OFF, { [`GET ${WS}/sending-domains`]: () => problem(500, "Broken", "no domains") }));
    mount();

    const form = await screen.findByRole("form", { name: "UTM parameters" });
    expect(await within(form).findByText("no domains")).toBeInTheDocument();
    expect(within(form).getByRole("button", { name: "Try again" })).toBeInTheDocument();
  });

  it("shows an error state with a retry when the workspace cannot be loaded", async () => {
    mockApi(routes("admin", UTM_OFF, { [`GET ${WS}`]: () => problem(500, "Broken", "no workspace") }));
    mount();

    expect(await screen.findAllByRole("button", { name: "Try again" })).not.toHaveLength(0);
  });
});

describe("UTM pattern helpers", () => {
  it("names an unknown placeholder and accepts the fixed set", () => {
    expect(checkPattern("{workspace}-{flow_version}")).toBeUndefined();
    expect(checkPattern("{person.email}")).toContain("{person.email}");
    expect(checkPattern("{flow")).toBeDefined();
    expect(checkPattern("flow}")).toBeDefined();
  });

  it("slugs and encodes the example values and leaves an empty parameter out", () => {
    const link = exampleLink(
      { enabled: true, source: "{workspace}", medium: "e mail", campaign: "{step}", content: "", domains: ["a.example.com"] },
      "Acme Corp",
    );
    expect(link).toBe("https://a.example.com/offer?utm_source=acme-corp&utm_medium=e%20mail&utm_campaign=send-welcome");
  });

  it("refuses the C1 control characters like the server", () => {
    expect(checkPattern("ab")).toBeDefined();
    expect(checkPattern("ab")).toBeDefined();
    expect(checkPattern("a b")).toBeUndefined();
  });

  it("drops every trailing dot of a domain and refuses a host over 253 bytes", () => {
    expect(parseDomains("example.com..")).toEqual(["example.com"]);
    const long = `${["a".repeat(63), "b".repeat(63), "c".repeat(63), "d".repeat(58)].join(".")}.com`;
    expect(long.length).toBe(254);
    const draft = { enabled: false, source: "", medium: "", campaign: "", content: "", domains: [long] };
    expect(checkUtm(draft).domains).toBeDefined();
    expect(checkUtm({ ...draft, domains: [long.slice(1)] }).domains).toBeUndefined();
  });

  it("reads domains one per line or comma separated", () => {
    expect(parseDomains("Example.com\n*.shop.example.com, example.com .news.example.org.")).toEqual([
      "example.com",
      "shop.example.com",
      "news.example.org",
    ]);
  });
});
