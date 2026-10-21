import type { components } from "../../api";
import { Pill, type Tone } from "../../ui";
import { wordsFor, type Names } from "./describe";
import { kindLabel, TRIGGER_ID, type Graph } from "./model";

type Simulation = components["schemas"]["SimulationBody"];
type Person = components["schemas"]["SimulatedPersonBody"];

/** A step as the canvas names it: its kind and its words. The caller keeps the id beside it. */
export function stepTitle(graph: Graph | null, names: Names, id: string): string {
  if (id === TRIGGER_ID) return "Trigger";
  const node = graph?.nodes.find((candidate) => candidate.id === id);
  return node ? `${kindLabel(node.type)}: ${wordsFor(node, names).title}` : "A step that is not in the flow";
}

const STATUS: Record<string, { text: string; tone: Tone }> = {
  completed: { text: "Completed", tone: "neutral" },
  exited: { text: "Exited", tone: "neutral" },
  waiting: { text: "Waiting", tone: "attention" },
  failed: { text: "Failed", tone: "error" },
};

const REFUSED: Record<string, string> = {
  step_limit: "Stopped: too many steps",
  no_entry: "Stopped: no first step",
};

const LABELS: Record<string, string> = {
  blank: "Nothing known about them",
  first_case: "Fits the first condition",
  last_case: "Fits the last condition",
};

const NOT_SIMULATED: Record<string, string> = {
  goal_segment: "A goal that is reaching a segment is not checked.",
};

function statusOf(person: Person): { text: string; tone: Tone } {
  if (person.refused) return { text: REFUSED[person.refused] ?? "Stopped", tone: "error" };
  const known = person.final_status ? STATUS[person.final_status] : undefined;
  return known ?? { text: person.final_status ?? "No result", tone: "neutral" };
}

function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

interface Props {
  simulation: Simulation;
  /** The drafted flow, to name its steps; null when it cannot be read. */
  graph: Graph | null;
  names: Names;
}

/**
 * What a few made-up people do in the drafted flow, from the server's simulation: the path of each,
 * where it ends, what it would have changed or called, and the steps nobody reached. Nothing was run
 * for real, and the summary says so.
 */
export function SimulationSummary({ simulation, graph, names }: Props) {
  const title = (id: string) => stepTitle(graph, names, id);
  return (
    <div className="stack-tight" role="group" aria-label="Simulation of the draft">
      <p className="faint">Made-up people walked through the draft on a virtual clock. Nothing was sent or changed.</p>
      {simulation.people.length === 0 ? (
        <p className="muted">No one could be walked through this draft.</p>
      ) : (
        <ul className="list-plain stack-tight" aria-label="Made-up people">
          {simulation.people.map((person, index) => {
            const status = statusOf(person);
            return (
              <li key={`${person.label}-${index}`}>
                <details>
                  <summary>
                    <span>{person.description}</span> <Pill tone={status.tone}>{status.text}</Pill>
                  </summary>
                  <div className="stack-tight">
                    <p className="faint">{LABELS[person.label] ?? person.label}</p>
                    {person.path.length === 0 ? (
                      <p className="muted">This person ran no step.</p>
                    ) : (
                      <ol className="list-plain" aria-label={`Path of ${person.description}`}>
                        {person.path.map((id, step) => (
                          <li key={`${step}-${id}`}>
                            {step + 1}. {title(id)} <span className="faint mono">{id}</span>
                          </li>
                        ))}
                      </ol>
                    )}
                    {person.halted ? <p className="muted">Stopped early: {person.halted.replaceAll("_", " ")}.</p> : null}
                    <p className="muted">
                      Would change {plural(person.internal_effects, "thing", "things")} about them and make{" "}
                      {plural(person.external_effects, "outward call", "outward calls")}.
                    </p>
                  </div>
                </details>
              </li>
            );
          })}
        </ul>
      )}
      <div>
        <h6 className="strong">Steps no one reached</h6>
        {simulation.unreached_nodes.length === 0 ? (
          <p className="muted">Every step was reached by at least one of them.</p>
        ) : (
          <ul className="list-plain" aria-label="Steps no one reached">
            {simulation.unreached_nodes.map((id) => (
              <li key={id}>
                {title(id)} <span className="faint mono">{id}</span>
              </li>
            ))}
          </ul>
        )}
      </div>
      {simulation.not_simulated.length > 0 ? (
        <ul className="list-plain faint" aria-label="What the simulation does not check">
          {simulation.not_simulated.map((code) => (
            <li key={code}>{NOT_SIMULATED[code] ?? `Not checked: ${code.replaceAll("_", " ")}.`}</li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}
