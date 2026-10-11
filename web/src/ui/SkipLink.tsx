export function SkipLink({ target }: { target: string }) {
  return (
    <a className="skip-link" href={`#${target}`}>
      Skip to main content
    </a>
  );
}
