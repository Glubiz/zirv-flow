import { describe, expect, it } from "vitest";

import { destinationUrl, isFlowEditor, isSettingsPath, matchesDestination, NAVIGATION, visibleDestinations } from "./navigation";

const base = "/w/ws1";
const item = (route: string) => NAVIGATION.find((entry) => entry.route === route)!;

describe("navigation permissions", () => {
  it("offers admin destinations to admins and reserves Jobs for the owner", () => {
    const admin = visibleDestinations({ canAdmin: true, isOwner: false });
    for (const route of ["sending-domains", "destinations", "forwarding", "privacy", "settings/audit", "api-keys", "credentials", "ai"]) {
      expect(admin).toContainEqual(item(route));
    }
    expect(admin).not.toContainEqual(item("/jobs"));
    expect(visibleDestinations({ canAdmin: true, isOwner: true })).toContainEqual(item("/jobs"));
  });

  it("hides restricted destinations from readers and keeps readable settings", () => {
    const reader = visibleDestinations({ canAdmin: false, isOwner: false });
    expect(reader.every((entry) => !entry.role)).toBe(true);
    for (const route of ["members", "traits", "contact-definition", "settings/sending", "events/settings", "/health"]) {
      expect(reader).toContainEqual(item(route));
    }
  });

  it("disables workspace destinations without a workspace while global pages remain reachable", () => {
    for (const entry of NAVIGATION) {
      expect(destinationUrl(entry, null)).toBe(entry.global ? entry.route : null);
    }
  });
});

describe("navigation matches", () => {
  it.each(["topics", "consent/dimensions", "consent/policy", "suppressions"])("keeps Consent current on %s", (route) => {
    expect(matchesDestination(item("topics"), `${base}/${route}`, base)).toBe(true);
  });

  it.each(["themes", "themes/t1", "snippets/s1", "images"])("keeps Content library current on %s", (route) => {
    expect(matchesDestination(item("themes"), `${base}/${route}`, base)).toBe(true);
  });

  it("matches complete segments, descendants and the exact Home address", () => {
    expect(matchesDestination(item(""), base, base)).toBe(true);
    expect(matchesDestination(item(""), `${base}/`, base)).toBe(true);
    expect(matchesDestination(item(""), `${base}/people`, base)).toBe(false);
    expect(matchesDestination(item("people"), `${base}/people/conflicts`, base)).toBe(true);
    expect(matchesDestination(item("people"), `${base}/people/p1`, base)).toBe(true);
    expect(matchesDestination(item("people"), `${base}/people-extra`, base)).toBe(false);
    expect(matchesDestination(item("people"), "/w/ws2/people", base)).toBe(false);
  });

  it("separates Explorer and Event settings from Events", () => {
    expect(matchesDestination(item("events"), `${base}/events/rejects`, base)).toBe(true);
    for (const route of ["events/explorer", "events/settings"]) {
      expect(matchesDestination(item("events"), `${base}/${route}`, base)).toBe(false);
      expect(matchesDestination(item(route), `${base}/${route}`, base)).toBe(true);
    }
  });

  it("recognizes every settings URL, including detail and global routes", () => {
    for (const entry of NAVIGATION.filter((entry) => entry.area === "settings" || entry.route === "settings")) {
      expect(isSettingsPath(destinationUrl(entry, base)!, base)).toBe(true);
    }
    expect(isSettingsPath(`${base}/destinations/d1`, base)).toBe(true);
    expect(isSettingsPath(`${base}/sending-domains/d1`, base)).toBe(true);
    expect(isSettingsPath(`${base}/events/explorer`, base)).toBe(false);
    expect(isSettingsPath(`${base}/settings-extra`, base)).toBe(false);
  });

  it("only auto-folds the flow editor, including a trailing slash", () => {
    expect(isFlowEditor(`${base}/flows/f1`)).toBe(true);
    expect(isFlowEditor(`${base}/flows/f1/`)).toBe(true);
    for (const route of ["flows", "flows/new", "flows/new/", "templates/t1"]) {
      expect(isFlowEditor(`${base}/${route}`)).toBe(false);
    }
  });
});
