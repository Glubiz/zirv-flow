const PATHS: Record<string, string> = {
  Home: "M3 11 12 4l9 7M5 10v10h14V10",
  Audience: "M15 20c0-7-12-7-12 0M9 11a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7M17 5c5 0 5 7 0 7m0 3c3 0 4 2 4 5",
  Data: "M4 6c0-4 16-4 16 0s-16 4-16 0m0 0v12c0 4 16 4 16 0V6M4 12c0 4 16 4 16 0",
  Messaging: "m21 3-7 18-4-7-7-4 18-7-11 11",
  Insights: "M4 20V10m7 10V4m7 16v-7",
  Workspace: "M4 21V4h16v17M9 8h6M9 12h6M9 21v-5h6v5",
  Sending: "m3 4 18 8-18 8 3-8-3-8m3 8h15",
  Events: "m13 2-9 12h7l-1 8 10-13h-7l1-7",
  Integrations: "M8 3v5m8-5v5M5 8h14v4a7 7 0 0 1-14 0V8m7 11v3",
  Compliance: "m12 3 8 3v6c0 5-8 9-8 9s-8-4-8-9V6l8-3m-4 9 3 3 5-6",
  System: "M3 4h18v12H3V4m5 17h8m-4-5v5",
  Settings: "M4 6h16M4 12h16M4 18h16M8 3v6m8 0v6m-6 0v6",
  Search: "M16 16l5 5M10 17a7 7 0 1 0 0-14 7 7 0 0 0 0 14",
  Fold: "m14 6-6 6 6 6M20 4v16",
  Unfold: "m10 6 6 6-6 6M4 4v16",
  Menu: "M4 6h16M4 12h16M4 18h16",
  Back: "m14 6-6 6 6 6",
};

export function NavigationIcon({ name }: { name: string }) {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d={PATHS[name] ?? PATHS.Settings} />
    </svg>
  );
}
