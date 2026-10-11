import { useId, type InputHTMLAttributes, type SelectHTMLAttributes } from "react";

interface TextFieldProps extends Omit<InputHTMLAttributes<HTMLInputElement>, "id"> {
  label: string;
  hint?: string;
  mono?: boolean;
}

export function TextField({ label, hint, mono = false, className, ...rest }: TextFieldProps) {
  const id = useId();
  const hintId = `${id}-hint`;
  const classes = ["input", mono ? "input-mono" : "", className ?? ""].filter(Boolean).join(" ");
  return (
    <div className="field">
      <label className="field-label" htmlFor={id}>
        {label}
      </label>
      <input id={id} className={classes} aria-describedby={hint ? hintId : undefined} {...rest} />
      {hint ? (
        <span className="field-hint" id={hintId}>
          {hint}
        </span>
      ) : null}
    </div>
  );
}

interface SelectFieldProps extends Omit<SelectHTMLAttributes<HTMLSelectElement>, "id"> {
  label: string;
  hint?: string;
}

export function SelectField({ label, hint, className, children, ...rest }: SelectFieldProps) {
  const id = useId();
  const hintId = `${id}-hint`;
  return (
    <div className="field">
      <label className="field-label" htmlFor={id}>
        {label}
      </label>
      <select
        id={id}
        className={["select", className ?? ""].filter(Boolean).join(" ")}
        aria-describedby={hint ? hintId : undefined}
        {...rest}
      >
        {children}
      </select>
      {hint ? (
        <span className="field-hint" id={hintId}>
          {hint}
        </span>
      ) : null}
    </div>
  );
}
