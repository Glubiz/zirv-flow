import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { destination, destinationKinds, DEST_ID, WS } from "../../test/destinationsApi";
import { json, mockApi, problem } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { actor } from "../../test/segmentsApi";
import { DestinationsPage } from "./DestinationsPage";

const SECRET = "whsec_test_0123456789abcdef";

function mount() {
  return renderRoute("/w/ws1/destinations", "/w/:ws/destinations", <DestinationsPage />);
}

const NOT_SET = { configured: false, created_at: null, grace_seconds: 86400, previous_valid_until: null };
const SET = { configured: true, created_at: "2026-10-02T09:00:00Z", grace_seconds: 86400, previous_valid_until: null };

function base(extra: Record<string, () => Response | Promise<Response>> = {}) {
  return {
    ...actor("writer"),
    "GET /api/v1/destination-kinds": () => json(destinationKinds()),
    [`GET ${WS}/destinations`]: () => json([destination()]),
    [`GET ${WS}/webhook-signing-secret`]: () => json(SET),
    ...extra,
  };
}

function stored(storage: Storage): string {
  return Array.from({ length: storage.length }, (_, at) => {
    const key = storage.key(at) ?? "";
    return `${key}=${storage.getItem(key) ?? ""}`;
  }).join("; ");
}

afterEach(() => {
  vi.unstubAllGlobals();
  window.localStorage.clear();
  window.sessionStorage.clear();
});

describe("destinations", () => {
  it("lists each destination with its type, credential, rate and a link to its page", async () => {
    mockApi(base());
    mount();

    const table = await screen.findByRole("table", { name: "Destinations of this workspace" });
    const row = within(table).getAllByRole("row")[1]!;
    expect(within(row).getByRole("link", { name: "Order hook" })).toHaveAttribute("href", `/w/ws1/destinations/${DEST_ID}`);
    expect(within(row).getByText("Webhook")).toBeInTheDocument();
    expect(within(row).getByText("Credential set")).toBeInTheDocument();
    expect(within(row).getByText("10 per second")).toBeInTheDocument();
    expect(screen.getByText("1 destination in all.")).toBeInTheDocument();
  });

  it("shows a loading state while the destinations load", async () => {
    mockApi(base({ [`GET ${WS}/destinations`]: () => new Promise<Response>(() => undefined) }));
    mount();

    expect(await screen.findByText("Loading destinations")).toBeInTheDocument();
  });

  it("shows an empty state that offers a first destination", async () => {
    mockApi(base({ [`GET ${WS}/destinations`]: () => json([]) }));
    mount();

    expect(await screen.findByText("No destinations yet")).toBeInTheDocument();
    expect(screen.getAllByRole("link", { name: "New destination" })).toHaveLength(2);
  });

  it("shows an error with a retry, and loads again", async () => {
    let attempts = 0;
    mockApi(
      base({
        [`GET ${WS}/destinations`]: () => {
          attempts += 1;
          return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json([destination()]);
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("link", { name: "Order hook" })).toBeInTheDocument();
    expect(attempts).toBe(2);
  });

  it("sends someone who is not an admin away from the page", async () => {
    mockApi({ ...base(), ...actor("reader") });
    const { router } = mount();

    await waitFor(() => expect(router.state.location.pathname).toBe("/w/ws1/members"));
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
    expect(screen.queryByText("Webhook signing secret")).not.toBeInTheDocument();
  });
});

describe("the webhook signing secret", () => {
  it("shows the secret once when it is created, with a copy button and no way to read it back", async () => {
    let configured = false;
    const calls = mockApi(
      base({
        [`GET ${WS}/webhook-signing-secret`]: () => json(configured ? SET : NOT_SET),
        [`POST ${WS}/webhook-signing-secret`]: () => {
          configured = true;
          return json({ secret: SECRET, created_at: "2026-10-02T09:00:00Z", grace_seconds: 86400, previous_valid_until: null }, 201);
        },
      }),
    );
    const user = userEvent.setup();
    const first = mount();

    expect(await screen.findByText("Not set up")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Create the secret" }));

    const dialog = await screen.findByRole("dialog", { name: "Your signing secret" });
    expect(within(dialog).getByText(SECRET)).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Copy the signing secret" })).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "I have saved the secret" }));
    expect(screen.queryByText(SECRET)).not.toBeInTheDocument();
    expect(await screen.findByText("Configured")).toBeInTheDocument();

    // A reload of the page: nothing returns the secret, and nothing kept it.
    first.unmount();
    mount();
    expect(await screen.findByText("Configured")).toBeInTheDocument();
    expect(screen.queryByText(SECRET)).not.toBeInTheDocument();
    expect(document.body.textContent).not.toContain(SECRET);
    expect(stored(window.localStorage)).not.toContain(SECRET);
    expect(stored(window.sessionStorage)).not.toContain(SECRET);
    expect(window.location.href).not.toContain(SECRET);
    expect(calls.filter((call) => call.method === "POST")).toHaveLength(1);
  });

  it("asks before rotating, shows the new secret once and says how long the old one keeps signing", async () => {
    const calls = mockApi(
      base({
        [`POST ${WS}/webhook-signing-secret/rotate`]: () =>
          json({ secret: SECRET, created_at: "2026-10-03T09:00:00Z", grace_seconds: 86400, previous_valid_until: "2026-10-04T09:00:00Z" }, 201),
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Rotate the secret" }));
    const confirm = await screen.findByRole("dialog", { name: "Rotate the signing secret?" });
    expect(within(confirm).getByText(/keeps signing for 24 hours/)).toBeInTheDocument();
    expect(calls.some((call) => call.method === "POST")).toBe(false);
    await user.click(within(confirm).getByRole("button", { name: "Rotate now" }));

    const dialog = await screen.findByRole("dialog", { name: "Your signing secret" });
    expect(within(dialog).getByText(SECRET)).toBeInTheDocument();
    expect(within(dialog).getByText(/The previous secret keeps signing for 24 hours/)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "I have saved the secret" }));
    expect(screen.queryByText(SECRET)).not.toBeInTheDocument();
  });
});
