import { useId, type InputHTMLAttributes, type SelectHTMLAttributes, type TextareaHTMLAttributes } from "react";

/** The hint and the error of a field, wired to the control for assistive technology. */
function describedBy(id: string, hint: string | undefined, error: string | undefined): string | undefined {
  const ids = [hint ? `${id}-hint` : null, error ? `${id}-error` : null].filter(Boolean);
  return ids.length > 0 ? ids.join(" ") : undefined;
}

function Notes({ id, hint, error }: { id: string; hint?: string; error?: string }) {
  return (
    <>
      {hint ? (
        <span className="field-hint" id={`${id}-hint`}>
          {hint}
        </span>
      ) : null}
      {error ? (
        <span className="field-error" id={`${id}-error`}>
          {error}
        </span>
      ) : null}
    </>
  );
}

interface TextFieldProps extends Omit<InputHTMLAttributes<HTMLInputElement>, "id"> {
  label: string;
  hint?: string;
  /** What is wrong with the value; marks the field invalid. */
  error?: string;
  mono?: boolean;
}

export function TextField({ label, hint, error, mono = false, className, ...rest }: TextFieldProps) {
  const id = useId();
  const classes = ["input", mono ? "input-mono" : "", className ?? ""].filter(Boolean).join(" ");
  return (
    <div className="field">
      <label className="field-label" htmlFor={id}>
        {label}
      </label>
      <input
        id={id}
        className={classes}
        aria-describedby={describedBy(id, hint, error)}
        aria-invalid={error ? true : undefined}
        {...rest}
      />
      <Notes id={id} hint={hint} error={error} />
    </div>
  );
}

interface SelectFieldProps extends Omit<SelectHTMLAttributes<HTMLSelectElement>, "id"> {
  label: string;
  hint?: string;
  /** What is wrong with the value; marks the field invalid. */
  error?: string;
}

export function SelectField({ label, hint, error, className, children, ...rest }: SelectFieldProps) {
  const id = useId();
  return (
    <div className="field">
      <label className="field-label" htmlFor={id}>
        {label}
      </label>
      <select
        id={id}
        className={["select", className ?? ""].filter(Boolean).join(" ")}
        aria-describedby={describedBy(id, hint, error)}
        aria-invalid={error ? true : undefined}
        {...rest}
      >
        {children}
      </select>
      <Notes id={id} hint={hint} error={error} />
    </div>
  );
}

interface TextAreaFieldProps extends Omit<TextareaHTMLAttributes<HTMLTextAreaElement>, "id"> {
  label: string;
  hint?: string;
  mono?: boolean;
}

export function TextAreaField({ label, hint, mono = false, className, ...rest }: TextAreaFieldProps) {
  const id = useId();
  const classes = ["input", "textarea", mono ? "input-mono" : "", className ?? ""].filter(Boolean).join(" ");
  return (
    <div className="field">
      <label className="field-label" htmlFor={id}>
        {label}
      </label>
      <textarea id={id} className={classes} aria-describedby={describedBy(id, hint, undefined)} {...rest} />
      <Notes id={id} hint={hint} />
    </div>
  );
}
