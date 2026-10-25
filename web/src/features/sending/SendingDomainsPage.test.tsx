import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { domain, DOMAIN_ID, record, WS } from "../../test/destinationsApi";
import { json, mockApi, problem } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { actor } from "../../test/segmentsApi";
import { SendingDomainsPage } from "./SendingDomainsPage";

function mount() {
  return renderRoute("/w/ws1/sending-domains", "/w/:ws/sending-domains", <SendingDomainsPage />);
}

describe("sending domains", () => {
  it("lists each domain with its status and how many records match", async () => {
    mockApi({
      ...actor("writer"),
      [`GET ${WS}/sending-domains`]: () =>
        json([
          domain(),
          domain({
            id: "other",
            domain: "news.example.test",
            status: "verified",
            records: [record("dkim", { status: "ok" }), record("spf", { status: "ok" }), record("dmarc", { status: "ok" })],
          }),
        ]),
    });
    mount();

    const table = await screen.findByRole("table", { name: "Sending domains of this workspace" });
    const rows = within(table).getAllByRole("row");
    expect(within(rows[1]!).getByRole("link", { name: "mail.example.test" })).toHaveAttribute("href", `/w/ws1/sending-domains/${DOMAIN_ID}`);
    expect(within(rows[1]!).getByText("Not verified")).toBeInTheDocument();
    expect(within(rows[1]!).getByText("1 of 3")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("Verified")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("3 of 3")).toBeInTheDocument();
  });

  it("shows a loading state while the domains load", async () => {
    mockApi({ ...actor("writer"), [`GET ${WS}/sending-domains`]: () => new Promise<Response>(() => undefined) });
    mount();

    expect(await screen.findByText("Loading sending domains")).toBeInTheDocument();
  });

  it("shows an empty state that offers a first domain", async () => {
    mockApi({ ...actor("writer"), [`GET ${WS}/sending-domains`]: () => json([]) });
    mount();

    expect(await screen.findByText("No sending domains yet")).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: "Add a domain" })).toHaveLength(2);
  });

  it("shows an error with a retry", async () => {
    let attempts = 0;
    mockApi({
      ...actor("writer"),
      [`GET ${WS}/sending-domains`]: () => {
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json([domain()]);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("link", { name: "mail.example.test" })).toBeInTheDocument();
  });

  it("registers a domain and opens its checklist", async () => {
    const calls = mockApi({
      ...actor("writer"),
      [`GET ${WS}/sending-domains`]: () => json([]),
      [`POST ${WS}/sending-domains`]: () => json(domain(), 201),
    });
    const user = userEvent.setup();
    const { router } = mount();

    await user.click((await screen.findAllByRole("button", { name: "Add a domain" }))[0]!);
    await user.type(screen.getByLabelText("Domain"), "mail.example.test");
    await user.click(screen.getByRole("button", { name: "Add the domain" }));

    await waitFor(() => expect(router.state.location.pathname).toBe(`/w/ws1/sending-domains/${DOMAIN_ID}`));
    expect(calls.find((call) => call.method === "POST")?.body).toEqual({ domain: "mail.example.test" });
  });

  it("sends someone who is not an admin away from the page", async () => {
    mockApi({ ...actor("reader"), [`GET ${WS}/sending-domains`]: () => json([domain()]) });
    const { router } = mount();

    await waitFor(() => expect(router.state.location.pathname).toBe("/w/ws1/members"));
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
  });
});
