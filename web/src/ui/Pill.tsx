import type { ReactNode } from "react";

export type Tone = "neutral" | "attention" | "error";

interface PillProps {
  tone?: Tone;
  children: ReactNode;
}

const DOTS: Record<Tone, string> = {
  neutral: "dot dot-quiet",
  attention: "dot dot-attention",
  error: "dot dot-error",
};

const PILLS: Record<Tone, string> = {
  neutral: "pill",
  attention: "pill pill-attention",
  error: "pill pill-error",
};

export function Pill({ tone = "neutral", children }: PillProps) {
  return (
    <span className={PILLS[tone]}>
      <span className={DOTS[tone]} aria-hidden="true" />
      {children}
    </span>
  );
}
