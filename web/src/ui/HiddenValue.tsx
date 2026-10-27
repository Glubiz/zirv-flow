/** A value the signed-in person may not see: shown as "Hidden", described for assistive technology. */
export function HiddenValue({ what = "This value" }: { what?: string }) {
  return (
    <span className="faint">
      Hidden<span className="visually-hidden">. {what} is hidden; seeing it needs the personal data permission.</span>
    </span>
  );
}
