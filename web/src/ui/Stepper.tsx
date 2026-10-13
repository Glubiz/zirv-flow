export interface StepEntry<S extends string> {
  step: S;
  label: string;
}

/** Where the person is in a short sequence; the current step is announced to screen readers. */
export function Stepper<S extends string>({
  steps,
  current,
  label,
}: {
  steps: readonly StepEntry<S>[];
  current: S;
  label: string;
}) {
  const at = steps.findIndex((entry) => entry.step === current);
  return (
    <ol className="stepper" aria-label={label}>
      {steps.map((entry, index) => (
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
