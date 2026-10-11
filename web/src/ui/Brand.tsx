import logoUrl from "./logo.png";

interface BrandProps {
  size?: 30 | 36;
}

export function Brand({ size = 30 }: BrandProps) {
  return (
    <span className={size === 36 ? "brand brand-large" : "brand"}>
      <img src={logoUrl} alt="" width={size} height={size} />
      <span className="brand-word">
        <b>zirv</b> <i>flow</i>
      </span>
    </span>
  );
}
