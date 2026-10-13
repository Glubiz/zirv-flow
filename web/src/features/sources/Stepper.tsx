export type Step = "type" | "settings" | "test" | "streams";

const STEPS: { step: Step; label: string }[] = [
  { step: "type", label: "Choose a type" },
  { step: "settings", label: "Settings" },
  { step: "test", label: "Test" },
  { step: "streams", label: "Streams" },
];

/** Where the person is in the four steps; the current one is announced to screen readers. */
export function Stepper({ current }: { current: Step }) {
  const at = STEPS.findIndex((entry) => entry.step === current);
  return (
    <ol className="stepper" aria-label="Source setup steps">
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
