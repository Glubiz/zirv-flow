/** A value the signed-in person may not see: shown as "Hidden", described for assistive technology. */
export function HiddenValue({ what = "This value" }: { what?: string }) {
  return (
    <span className="faint">
      Hidden<span className="visually-hidden">. {what} is hidden; only workspace admins can see it.</span>
    </span>
  );
}
