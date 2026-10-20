import { NIL_UUID, type Settings } from "./model";

export type TriggerType = Settings["trigger"]["type"];

export const TRIGGER_KINDS: readonly { type: TriggerType; label: string }[] = [
  { type: "segment", label: "A person enters or leaves a segment" },
  { type: "event", label: "An event happens" },
  { type: "schedule", label: "On a schedule" },
  { type: "manual", label: "By hand" },
  { type: "api", label: "From the API" },
];

function browserZone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
  } catch {
    return "UTC";
  }
}

/** A trigger of `type` with nothing chosen yet; validation names what is still missing. */
export function blankTrigger(type: TriggerType): Settings["trigger"] {
  switch (type) {
    case "segment":
      return { type, segment_id: NIL_UUID, on: "entered" };
    case "event":
      return { type, event: "" };
    case "schedule":
      return { type, cron: "0 9 * * *", timezone: browserZone(), audience: { type: "everyone" } };
    case "manual":
      return { type };
    case "api":
      return { type };
  }
}

/** The settings a new flow starts with: its trigger, and a person enters only once. */
export function defaultSettings(type: TriggerType): Settings {
  return { trigger: blankTrigger(type), reentry: { mode: "once" } };
}
