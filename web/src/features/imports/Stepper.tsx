export type Step = "upload" | "map" | "dry-run" | "run";

const STEPS: { step: Step; label: string }[] = [
  { step: "upload", label: "Upload" },
  { step: "map", label: "Map" },
  { step: "dry-run", label: "Dry run" },
  { step: "run", label: "Run" },
];

/** Where the person is in the four steps; the current one is announced to screen readers. */
export function Stepper({ current }: { current: Step }) {
  const at = STEPS.findIndex((entry) => entry.step === current);
  return (
    <ol className="stepper" aria-label="Import steps">
      {STEPS.map((entry, index) => (
        <li
          key={entry.step}
          className={index < at ? "stepper-done" : index === at ? "stepper-current" : undefined}
          aria-current={index === at ? "step" : undefined}
        >
          <span className="stepper-number" aria-hidden="true">
            {index + 1}
          </span>
          {entry.label}
          {index < at ? <span className="visually-hidden"> (done)</span> : null}
        </li>
      ))}
    </ol>
  );
}
