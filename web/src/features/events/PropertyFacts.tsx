/** The properties of an event: a masked value is shown as "masked", never as null. */
export function PropertyFacts({ properties, masked }: { properties: Record<string, unknown>; masked: readonly string[] }) {
  const entries = Object.entries(properties);
  const hidden = new Set(masked);
  if (entries.length === 0) {
    return <p className="faint">No properties.</p>;
  }
  return (
    <dl className="facts">
      {entries.map(([key, value]) => (
        <div key={key} className="fact">
          <dt className="mono">{key}</dt>
          <dd>{hidden.has(key) ? <span className="faint">masked</span> : <span className="mono">{JSON.stringify(value)}</span>}</dd>
        </div>
      ))}
    </dl>
  );
}
