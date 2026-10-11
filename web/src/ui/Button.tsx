import type { ButtonHTMLAttributes } from "react";

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: "primary" | "quiet";
  large?: boolean;
}

export function Button({ variant = "quiet", large = false, className, type, ...rest }: ButtonProps) {
  const classes = ["btn", variant === "primary" ? "btn-primary" : "", large ? "btn-large" : "", className ?? ""]
    .filter(Boolean)
    .join(" ");
  return <button type={type ?? "button"} className={classes} {...rest} />;
}
