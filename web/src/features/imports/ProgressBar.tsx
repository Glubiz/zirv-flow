import { percent } from "./labels";

interface ProgressBarProps {
  label: string;
  done: number;
  total: number;
}

export function ProgressBar({ label, done, total }: ProgressBarProps) {
  const value = percent(done, total);
  return (
    <div className="progress-wrap">
      <progress className="progress" max={100} value={value} aria-label={label} />
      <span className="progress-text">{value}%</span>
    </div>
  );
}
