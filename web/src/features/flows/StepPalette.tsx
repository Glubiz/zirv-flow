import { useState } from "react";

import { TextField } from "../../ui";
import { FlowIcon } from "./icons";
import { KINDS, type NodeType } from "./model";

const GROUPS = ["Timing", "Logic", "Actions"] as const;
function group(type: NodeType) {
  if (type.startsWith("wait.")) return "Timing";
  if (["branch", "split.random", "goal", "exit"].includes(type)) return "Logic";
  return "Actions";
}

export function StepPalette({ disabled, placement, onAdd }: {
  disabled: boolean;
  placement: string;
  onAdd: (type: NodeType) => void;
}) {
  const [filter, setFilter] = useState("");
  const kinds = KINDS.filter((kind) => `${kind.label} ${kind.hint}`.toLowerCase().includes(filter.trim().toLowerCase()));
  return (
    <aside className="fl-palette" aria-label="Step palette">
      <h2>Add a step</h2>
      <TextField label="Filter steps" type="search" value={filter} onChange={(event) => setFilter(event.target.value)} />
      <p className="faint">{placement}</p>
      {GROUPS.map((name) => {
        const choices = kinds.filter((kind) => group(kind.type) === name);
        return choices.length ? (
          <section key={name} aria-label={name} className="stack-tight">
            <h3>{name}</h3>
            {choices.map((kind) => (
              <button key={kind.type} className="fl-palette-step" type="button" disabled={disabled}
                aria-label={kind.label} title={kind.hint} onClick={() => onAdd(kind.type)}>
                <FlowIcon name={kind.type} />
                <span>{kind.label}</span>
              </button>
            ))}
          </section>
        ) : null;
      })}
      {kinds.length === 0 ? <p role="status" className="muted">No steps match that filter.</p> : null}
    </aside>
  );
}
