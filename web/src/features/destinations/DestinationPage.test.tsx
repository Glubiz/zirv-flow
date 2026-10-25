import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { credential, CRED_ID, destination, DEST_ID, domain, DOMAIN_ID, queue, WS } from "../../test/destinationsApi";
import { json, mockApi, problem } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { actor } from "../../test/segmentsApi";
import { DestinationPage } from "./DestinationPage";

function mount() {
  return renderRoute(`/w/ws1/destinations/${DEST_ID}`, "/w/:ws/destinations/:id", <DestinationPage />);
}

type Route = () => Response | Promise<Response>;

function base(extra: Record<string, Route> = {}) {
  return {
    ...actor("writer"),
    [`GET ${WS}/destinations/${DEST_ID}`]: () => json(destination()),
    [`GET ${WS}/forwarding/status`]: () => json({ destinations: [queue()] }),
    [`GET ${WS}/credentials`]: () => json([credential(CRED_ID, "Hook secret", ["signing_secret"])]),
    ...extra,
  };
}

afterEach(() => vi.unstubAllGlobals());

describe("one destination", () => {
  it("shows the queue depth, the oldest lag and the dead letters", async () => {
    mockApi(base());
    mount();

    const panel = await screen.findByRole("region", { name: "Delivery" });
    expect(await within(panel).findByText("4")).toBeInTheDocument();
    expect(within(panel).getByText("2 min")).toBeInTheDocument();
    expect(within(panel).getByText("2")).toBeInTheDocument();
    expect(within(panel).getByText("10 per second, up to 20 at once")).toBeInTheDocument();
    expect(within(panel).getByRole("link", { name: "Open the dead letters" })).toHaveAttribute("href", "/w/ws1/forwarding");
  });

  it("saves a new rate and burst and shows the saved values after the reload", async () => {
    let stored = destination();
    const calls = mockApi(
      base({
        [`GET ${WS}/destinations/${DEST_ID}`]: () => json(stored),
        [`PATCH ${WS}/destinations/${DEST_ID}`]: () => {
          stored = destination({ rate: 25, burst: 60, updated_at: "2026-10-03T09:00:00Z" });
          return json(stored);
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    const rate = await screen.findByLabelText("Rate (deliveries per second)");
    expect(rate).toHaveValue(10);
    expect(screen.getByLabelText("Burst (most deliveries at once)")).toHaveValue(20);
    await user.clear(rate);
    await user.type(rate, "25");
    const burst = screen.getByLabelText("Burst (most deliveries at once)");
    await user.clear(burst);
    await user.type(burst, "60");
    await user.click(screen.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PATCH")).toBe(true));
    expect(calls.find((call) => call.method === "PATCH")?.body).toMatchObject({ rate: 25, burst: 60, credential_id: CRED_ID });
    expect(await screen.findByText("Saved.")).toBeInTheDocument();
    await waitFor(() => expect(screen.getByLabelText("Rate (deliveries per second)")).toHaveValue(25));
    expect(screen.getByLabelText("Burst (most deliveries at once)")).toHaveValue(60);
    expect(await screen.findByText("25 per second, up to 60 at once")).toBeInTheDocument();
  });

  it("refuses a rate outside the allowed range before it asks the server", async () => {
    const calls = mockApi(base());
    const user = userEvent.setup();
    mount();

    const rate = await screen.findByLabelText("Rate (deliveries per second)");
    await user.clear(rate);
    await user.type(rate, "0");

    expect(screen.getByText("A whole number from 1 to 100000.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save changes" })).toBeDisabled();
    expect(calls.some((call) => call.method === "PATCH")).toBe(false);
  });

  it("shows a passed check, and a failed one with its reason and code", async () => {
    let answer: Record<string, unknown> = { ok: true, code: null, error: null };
    mockApi(base({ [`POST ${WS}/destinations/${DEST_ID}/check`]: () => json(answer) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Check the destination" }));
    expect(await screen.findByText("Check passed")).toBeInTheDocument();

    answer = { ok: false, code: "credential_refused", error: "The credential was refused." };
    await user.click(screen.getByRole("button", { name: "Check the destination" }));
    expect(await screen.findByText("Check failed")).toBeInTheDocument();
    expect(screen.getByText("The credential was refused.")).toBeInTheDocument();
    expect(screen.getByText("(credential_refused)")).toBeInTheDocument();
  });

  it("shows the provider's domain status beside ours for a provider destination", async () => {
    mockApi(
      base({
        [`GET ${WS}/destinations/${DEST_ID}`]: () => json(destination({ kind: "provider", config: { manifest: { version: 1 } } })),
        [`GET ${WS}/sending-domains`]: () => json([domain()]),
        [`GET ${WS}/destinations/${DEST_ID}/domain-status`]: () =>
          json({
            domain: "mail.example.test",
            domain_id: DOMAIN_ID,
            status: "unverified",
            provider_only: [],
            records: [
              { kind: "dkim", host: "zirv._domainkey.mail.example.test", status: "ok", provider: { record_type: "TXT", status: "verified" } },
              { kind: "spf", host: "mail.example.test", status: "mismatch", provider: { record_type: "TXT", status: "pending" } },
            ],
          }),
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.selectOptions(await screen.findByLabelText("Sending domain"), DOMAIN_ID);

    const table = await screen.findByRole("table", { name: /Records of mail.example.test/ });
    expect(within(table).getByText("Verified at the provider")).toBeInTheDocument();
    expect(within(table).getByText("Pending at the provider")).toBeInTheDocument();
    expect(within(table).getByText("mismatch")).toBeInTheDocument();
    expect(screen.getByText("Not verified")).toBeInTheDocument();
  });

  it("says so when the provider's manifest has no domain-status request", async () => {
    mockApi(
      base({
        [`GET ${WS}/destinations/${DEST_ID}`]: () => json(destination({ kind: "provider", config: { manifest: { version: 1 } } })),
        [`GET ${WS}/sending-domains`]: () => json([domain()]),
        [`GET ${WS}/destinations/${DEST_ID}/domain-status`]: () =>
          problem(404, "Not Found", "no domain status", undefined, "domain_status_unsupported"),
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.selectOptions(await screen.findByLabelText("Sending domain"), DOMAIN_ID);

    expect(await screen.findByText(/declares no domain-status request/)).toBeInTheDocument();
  });

  it("shows a loading state while the destination loads", async () => {
    mockApi(base({ [`GET ${WS}/destinations/${DEST_ID}`]: () => new Promise<Response>(() => undefined) }));
    mount();

    expect(await screen.findByText("Loading the destination")).toBeInTheDocument();
  });

  it("shows an error with a retry", async () => {
    let attempts = 0;
    mockApi(
      base({
        [`GET ${WS}/destinations/${DEST_ID}`]: () => {
          attempts += 1;
          return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json(destination());
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("heading", { name: "Order hook" })).toBeInTheDocument();
  });

  it("sends someone who is not an admin away from the page", async () => {
    mockApi({ ...base(), ...actor("reader") });
    const { router } = mount();

    await waitFor(() => expect(router.state.location.pathname).toBe("/w/ws1/members"));
    expect(screen.queryByRole("heading", { name: "Order hook" })).not.toBeInTheDocument();
  });
});
