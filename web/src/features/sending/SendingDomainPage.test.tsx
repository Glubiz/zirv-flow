import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { domain, DOMAIN_ID, identity, record, WS } from "../../test/destinationsApi";
import { json, mockApi, problem } from "../../test/mockApi";
import { actor } from "../../test/segmentsApi";
import { renderRoute } from "../../test/renderFeature";
import { SendingDomainPage, timing } from "./SendingDomainPage";

const PATH = `/w/ws1/sending-domains/${DOMAIN_ID}`;

function mount() {
  return renderRoute(PATH, "/w/:ws/sending-domains/:id", <SendingDomainPage />);
}

function routes(extra: Record<string, () => Response | Promise<Response>> = {}) {
  return {
    ...actor("writer"),
    [`GET ${WS}/sending-domains/${DOMAIN_ID}`]: () => json(domain()),
    [`GET ${WS}/sender-identities`]: () => json([identity()]),
    ...extra,
  };
}

const original = { ...timing };
beforeEach(() => {
  timing.recheckMs = 5;
  timing.recheckTries = 20;
});
afterEach(() => {
  Object.assign(timing, original);
  vi.unstubAllGlobals();
});

describe("sending domain checklist", () => {
  it("shows each record with its status in words, the value to publish and what was found", async () => {
    mockApi(routes());
    mount();

    expect(await screen.findByRole("heading", { name: "mail.example.test" })).toBeInTheDocument();
    expect(screen.getByText("Not verified · 1 of 3 records match")).toBeInTheDocument();

    const dkim = screen.getByRole("region", { name: "DKIM record" });
    expect(within(dkim).getByText("OK: matches")).toBeInTheDocument();
    const spf = screen.getByRole("region", { name: "SPF record" });
    expect(within(spf).getByText("Mismatch: a different value is published")).toBeInTheDocument();
    expect(within(spf).getByText("v=spf1 a:smtp.example.test ~all")).toBeInTheDocument();
    expect(within(spf).getByText("We found")).toBeInTheDocument();
    expect(within(spf).getByText("v=spf1 include:_spf.relay.test -all")).toBeInTheDocument();
    const dmarc = screen.getByRole("region", { name: "DMARC record" });
    expect(within(dmarc).getByText("Missing: no record at this host")).toBeInTheDocument();
    expect(within(dmarc).getByRole("button", { name: "Copy the DMARC value" })).toBeInTheDocument();
    expect(within(dmarc).getByRole("button", { name: "Copy the DMARC host" })).toBeInTheDocument();
  });

  it("offers the merged SPF record when another SPF record is already published", async () => {
    mockApi(routes());
    mount();

    const spf = await screen.findByRole("region", { name: "SPF record" });
    expect(within(spf).getByText("v=spf1 include:_spf.relay.test a:smtp.example.test -all")).toBeInTheDocument();
    expect(within(spf).getByRole("button", { name: "Copy the merged SPF value" })).toBeInTheDocument();
  });

  it("checks DNS again and shows the records the new check found", async () => {
    let checked = false;
    const calls = mockApi(
      routes({
        [`GET ${WS}/sending-domains/${DOMAIN_ID}`]: () =>
          json(
            checked
              ? domain({
                  status: "verified",
                  last_checked_at: "2026-10-02T07:00:00Z",
                  records: [
                    record("dkim", { status: "ok" }),
                    record("spf", { status: "ok" }),
                    record("dmarc", { status: "ok" }),
                  ],
                })
              : domain(),
          ),
        [`POST ${WS}/sending-domains/${DOMAIN_ID}/verify`]: () => {
          checked = true;
          return json(domain());
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Check DNS now" }));

    expect(await screen.findByText("Verified")).toBeInTheDocument();
    expect(screen.queryByText("Mismatch: a different value is published")).not.toBeInTheDocument();
    expect(screen.queryByText("Missing: no record at this host")).not.toBeInTheDocument();
    expect(screen.getAllByText("OK: matches")).toHaveLength(3);
    expect(calls.filter((call) => call.method === "POST" && call.path.endsWith("/verify"))).toHaveLength(1);
  });

  it("shows sender identities as blocked, with the reason, while the domain is not verified", async () => {
    mockApi(routes());
    mount();

    const table = await screen.findByRole("table", { name: "Sender identities on mail.example.test" });
    expect(within(table).getByText("hello@mail.example.test")).toBeInTheDocument();
    expect(within(table).getByText("Blocked: domain not verified")).toBeInTheDocument();
    expect(within(table).queryByText("Can send")).not.toBeInTheDocument();
  });

  it("lets a sender identity send once the domain is verified", async () => {
    mockApi(
      routes({
        [`GET ${WS}/sending-domains/${DOMAIN_ID}`]: () =>
          json(domain({ status: "verified", records: [record("dkim", { status: "ok" }), record("spf", { status: "ok" }), record("dmarc", { status: "ok" })] })),
      }),
    );
    mount();

    expect(await screen.findByText("Can send")).toBeInTheDocument();
    expect(screen.queryByText(/Blocked/)).not.toBeInTheDocument();
  });

  it("adds a sender identity on the domain", async () => {
    const calls = mockApi(
      routes({ [`POST ${WS}/sender-identities`]: () => json(identity({ id: "new" }), 201) }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Add sender identity" }));
    await user.type(screen.getByLabelText("From name"), "News");
    await user.type(screen.getByLabelText("From address"), "news@mail.example.test");
    await user.click(screen.getByRole("button", { name: "Add the identity" }));

    await waitFor(() => expect(calls.some((call) => call.method === "POST" && call.path === `${WS}/sender-identities`)).toBe(true));
    const post = calls.find((call) => call.method === "POST" && call.path === `${WS}/sender-identities`);
    expect(post?.body).toEqual({ domain_id: DOMAIN_ID, from_name: "News", from_address: "news@mail.example.test" });
  });

  it("shows a loading state while the domain loads", async () => {
    mockApi(routes({ [`GET ${WS}/sending-domains/${DOMAIN_ID}`]: () => new Promise<Response>(() => undefined) }));
    mount();

    expect(await screen.findByText("Loading the sending domain")).toBeInTheDocument();
  });

  it("shows an error with a retry", async () => {
    let attempts = 0;
    mockApi(
      routes({
        [`GET ${WS}/sending-domains/${DOMAIN_ID}`]: () => {
          attempts += 1;
          return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json(domain());
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("heading", { name: "mail.example.test" })).toBeInTheDocument();
  });

  it("sends someone who is not an admin away from the page", async () => {
    mockApi({ ...routes(), ...actor("reader") });
    const { router } = mount();

    await waitFor(() => expect(router.state.location.pathname).toBe("/w/ws1/members"));
    expect(screen.queryByRole("heading", { name: "mail.example.test" })).not.toBeInTheDocument();
  });
});
