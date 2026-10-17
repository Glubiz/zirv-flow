import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, problem } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { actor, catalogRoutes, PERSON_ID, previewBody, previewed, refusal, WS } from "../../test/segmentsApi";
import { NewSegmentPage } from "./NewSegmentPage";

type Preview = (body: { definition: { all: unknown[] } }) => Response | Promise<Response>;

function mount(preview: Preview) {
  const calls = mockApi({
    ...actor("writer"),
    ...catalogRoutes(),
    [`POST ${WS}/segments/preview`]: async (request) => preview((await request.clone().json()) as { definition: { all: unknown[] } }),
  });
  renderRoute("/w/ws1/segments/new", "/w/:ws/segments/new", <NewSegmentPage />);
  return calls;
}

/** The headline count, which the page splits across two elements. */
function count(text: string) {
  return (_: string, element: Element | null) => element?.classList.contains("live-count") === true && element.textContent === text;
}

const pause = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

describe("live count", () => {
  it("asks for nothing and says so while a condition is unfinished", async () => {
    const calls = mount(() => json(previewBody()));
    const user = userEvent.setup();

    await screen.findByText(count("4 people match"));
    const before = previewed(calls).length;
    await user.click(screen.getByRole("button", { name: "Add condition to All of" }));

    expect(await screen.findByText("Finish the condition to see a count.")).toBeInTheDocument();
    await pause(900);
    expect(previewed(calls)).toHaveLength(before);
  });

  it("shows the count, the sample as links, and a definition that is always current as live", async () => {
    mount(() => json(previewBody(25)));

    expect(await screen.findByText(count("25 people match"))).toBeInTheDocument();
    expect(screen.getByText("Live: this reads only data that is always current.")).toBeInTheDocument();
    const sample = screen.getByRole("region", { name: "Sample of people" });
    expect(within(sample).getByRole("link", { name: "ada@example.test" })).toHaveAttribute("href", `/w/ws1/people/${PERSON_ID}`);
    expect(within(sample).getByText("Showing 1 person of 25.")).toBeInTheDocument();
  });

  it("shows when the data is from, and the sources that feed it", async () => {
    mount(() =>
      json(
        previewBody(2, {
          data_age: {
            live: false,
            as_of: "2026-10-10T08:00:00Z",
            sources: [
              { source_id: "s1", name: "Warehouse", last_success_at: "2026-10-10T08:00:00Z" },
              { source_id: "s2", name: "CRM", last_success_at: null },
            ],
          },
        }),
      ),
    );

    expect(await screen.findByText(/^Data as of /)).toBeInTheDocument();
    const sources = screen.getByRole("list", { name: "Sources that feed this" });
    expect(within(sources).getByText(/Warehouse: last succeeded /)).toBeInTheDocument();
    expect(within(sources).getByText("CRM: has never succeeded")).toBeInTheDocument();
  });

  it("says no one matches when the count is zero", async () => {
    mount(() => json(previewBody(0, { sample: [] })));

    expect(await screen.findByText("No one matches yet")).toBeInTheDocument();
    expect(await screen.findByText(count("0 people match"))).toBeInTheDocument();
  });

  it("shows a counting state before the first answer", async () => {
    mount(() => new Promise<Response>(() => undefined));

    expect(await screen.findByText("Counting people")).toBeInTheDocument();
  });

  it("keeps the last count on screen, marked as updating, while a new one is fetched", async () => {
    let answered = 0;
    mount(() => {
      answered += 1;
      return answered === 1 ? json(previewBody(4)) : new Promise<Response>(() => undefined);
    });
    const user = userEvent.setup();

    await screen.findByText(count("4 people match"));
    await user.click(screen.getByRole("button", { name: "Add group to All of" }));

    expect(await screen.findByText("Updating the count")).toBeInTheDocument();
    expect(screen.getByText(count("4 people match"))).toBeInTheDocument();
  });

  it("sends one request for several quick edits", async () => {
    const calls = mount(() => json(previewBody()));
    const user = userEvent.setup();

    await screen.findByText(count("4 people match"));
    expect(previewed(calls)).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "Add group to All of" }));
    await user.click(screen.getByRole("button", { name: "Add group to All of" }));
    await user.click(screen.getByRole("button", { name: "Add group to All of" }));

    await waitFor(() => expect(previewed(calls)).toHaveLength(2), { timeout: 4000 });
    await pause(900);
    expect(previewed(calls)).toHaveLength(2);
    expect(previewed(calls).at(-1)).toEqual({ v: 1, all: [{ all: [] }, { all: [] }, { all: [] }] });
  });

  it("ignores an answer that arrives after a newer edit", async () => {
    let release: (response: Response) => void = () => undefined;
    let asked = 0;
    mount(() => {
      asked += 1;
      return asked === 1 ? new Promise<Response>((resolve) => (release = resolve)) : json(previewBody(5));
    });
    const user = userEvent.setup();

    await waitFor(() => expect(asked).toBe(1));
    await user.click(await screen.findByRole("button", { name: "Add group to All of" }));
    expect(await screen.findByText(count("5 people match"),undefined, { timeout: 4000 })).toBeInTheDocument();

    release(json(previewBody(99)));
    await pause(200);

    expect(screen.getByText(count("5 people match"))).toBeInTheDocument();
    expect(screen.queryByText(count("99 people match"))).not.toBeInTheDocument();
  });

  it("offers a retry when the count times out, and counts again", async () => {
    let asked = 0;
    mount(() => {
      asked += 1;
      return asked === 1 ? refusal(504, "segment_preview_timeout") : json(previewBody(8));
    });
    const user = userEvent.setup();

    expect(await screen.findByText("The count took too long")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Try again" }));

    expect(await screen.findByText(count("8 people match"))).toBeInTheDocument();
    expect(asked).toBe(2);
  });

  it("marks the conditions a refused definition names instead of showing a count", async () => {
    mount((body) =>
      body.definition.all.length > 0
        ? refusal(422, "segment_invalid", [{ pointer: "/all/0/value", code: "value_missing" }])
        : json(previewBody()),
    );
    const user = userEvent.setup();

    await screen.findByText(count("4 people match"));
    await user.click(screen.getByRole("button", { name: "Add condition to All of" }));
    const condition = screen.getByRole("group", { name: "Condition 1" });
    await user.selectOptions(within(condition).getByLabelText("Field"), "city");
    await user.type(within(condition).getByLabelText("Value"), "Aarhus");

    expect(await screen.findByText("The definition has problems, marked on the conditions. Fix them to see a count.", undefined, { timeout: 4000 })).toBeInTheDocument();
    const value = within(condition).getByLabelText("Value");
    expect(value).toHaveAttribute("aria-invalid", "true");
    expect(value).toHaveAccessibleDescription("A value is needed.");
  });

  it("shows an error with a retry when the count fails", async () => {
    let asked = 0;
    mount(() => {
      asked += 1;
      return asked === 1 ? problem(500, "Internal Server Error", "internal error") : json(previewBody(3));
    });
    const user = userEvent.setup();

    expect(await screen.findByText("Internal Server Error")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Try again" }));

    expect(await screen.findByText(count("3 people match"))).toBeInTheDocument();
  });
});
