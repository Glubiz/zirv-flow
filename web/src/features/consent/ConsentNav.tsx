import { NavLink } from "react-router";

/** The pages of consent: what people can choose, how it is scoped, the rules on top, and who is blocked. */
export function ConsentNav({ workspaceId }: { workspaceId: string }) {
  const base = `/w/${workspaceId}`;
  return (
    <nav aria-label="Consent pages" className="subnav">
      <NavLink to={`${base}/topics`}>Topics</NavLink>
      <NavLink to={`${base}/consent/dimensions`}>Dimensions</NavLink>
      <NavLink to={`${base}/consent/policy`}>Policy</NavLink>
      <NavLink to={`${base}/suppressions`}>Suppressions</NavLink>
    </nav>
  );
}
