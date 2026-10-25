import { useCallback, useEffect, useState } from "react";

import { ApiError, client, describeError, unwrap, type DefinitionIssue, type ErrorSummary } from "../../api";
import {
  fingerprint,
  moveRule,
  newRule,
  rulesFromApi,
  rulesToRequest,
  sortIssues,
  type DraftRule,
  type RuleIssues,
} from "./policyModel";

type Loaded =
  | { status: "loading" }
  | { status: "error"; error: unknown }
  | { status: "ready" };

export interface PolicyDraft {
  load: Loaded;
  reload: () => void;
  rules: readonly DraftRule[];
  revision: number;
  /** The version the send gate evaluates; null before the first publish. */
  publishedVersion: number | null;
  dirty: boolean;
  busy: "saving" | "publishing" | null;
  /** The problems of the last refused save or publish, sorted onto the rules. */
  issues: RuleIssues;
  /** Something that is not an issue of one rule: a stale revision, a network failure. */
  failure: ErrorSummary | null;
  /** The draft was saved by someone else since this page read it. */
  stale: boolean;
  change: (key: string, next: Partial<Pick<DraftRule, "name" | "condition">>) => void;
  add: () => DraftRule;
  remove: (key: string) => void;
  move: (key: string, by: -1 | 1) => void;
  save: () => Promise<boolean>;
  publish: () => Promise<number | null>;
  discard: () => void;
}

const NO_ISSUES: RuleIssues = { general: [], rule: new Map(), condition: new Map() };

function issuesOf(error: unknown): DefinitionIssue[] | null {
  return error instanceof ApiError && error.code === "consent_policy_invalid" ? error.issues : null;
}

/** The draft of the workspace policy: edited in memory, saved with its revision, published at that revision. */
export function usePolicyDraft(workspaceId: string): PolicyDraft {
  const [load, setLoad] = useState<Loaded>({ status: "loading" });
  const [rules, setRules] = useState<DraftRule[]>([]);
  const [revision, setRevision] = useState(0);
  const [publishedVersion, setPublishedVersion] = useState<number | null>(null);
  const [saved, setSaved] = useState("");
  const [busy, setBusy] = useState<"saving" | "publishing" | null>(null);
  const [issues, setIssues] = useState<RuleIssues>(NO_ISSUES);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [stale, setStale] = useState(false);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    let live = true;
    unwrap(client.GET("/api/v1/workspaces/{ws}/consent/policy/draft", { params: { path: { ws: workspaceId } } })).then(
      (draft) => {
        if (!live) return;
        const next = rulesFromApi(draft.rules);
        setRules(next);
        setRevision(draft.revision);
        setPublishedVersion(draft.published_version ?? null);
        setSaved(fingerprint(next));
        setIssues(NO_ISSUES);
        setFailure(null);
        setStale(false);
        setLoad({ status: "ready" });
      },
      (error: unknown) => {
        if (live) setLoad({ status: "error", error });
      },
    );
    return () => {
      live = false;
    };
  }, [workspaceId, tick]);

  const reload = useCallback(() => {
    setLoad({ status: "loading" });
    setTick((value) => value + 1);
  }, []);

  function refuse(error: unknown) {
    const found = issuesOf(error);
    if (found !== null) {
      setIssues(sortIssues(found));
      setFailure(null);
      return;
    }
    if (error instanceof ApiError && error.code === "consent_policy_revision_mismatch") {
      setStale(true);
    }
    setFailure(describeError(error));
  }

  async function save(): Promise<boolean> {
    setBusy("saving");
    setFailure(null);
    setIssues(NO_ISSUES);
    const sentKeys = rules.map((rule) => rule.key);
    const sent = fingerprint(rules);
    try {
      const draft = await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/consent/policy/draft", {
          params: { path: { ws: workspaceId } },
          body: { revision, rules: rulesToRequest(rules) },
        }),
      );
      const next = rulesFromApi(draft.rules, sentKeys);
      // The new revision and baseline are always taken. The local rules are replaced by the stored
      // ones only if they are still what was sent; edits made while the save was in flight stay,
      // and are unsaved changes against the new baseline.
      setRules((current) =>
        fingerprint(current) === sent
          ? next
          : current.map((rule) => {
              const at = sentKeys.indexOf(rule.key);
              return rule.id === null && at >= 0 ? { ...rule, id: next[at]?.id ?? null } : rule;
            }),
      );
      setRevision(draft.revision);
      setPublishedVersion(draft.published_version ?? null);
      setSaved(fingerprint(next));
      return true;
    } catch (error) {
      refuse(error);
      return false;
    } finally {
      setBusy(null);
    }
  }

  async function publish(): Promise<number | null> {
    setBusy("publishing");
    setFailure(null);
    setIssues(NO_ISSUES);
    try {
      const version = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/consent/policy/publish", {
          params: { path: { ws: workspaceId } },
          body: { revision },
        }),
      );
      setPublishedVersion(version.version);
      return version.version;
    } catch (error) {
      refuse(error);
      return null;
    } finally {
      setBusy(null);
    }
  }

  function change(key: string, next: Partial<Pick<DraftRule, "name" | "condition">>) {
    setRules((current) => current.map((rule) => (rule.key === key ? { ...rule, ...next } : rule)));
  }

  function add(): DraftRule {
    setIssues(NO_ISSUES);
    const rule = newRule();
    setRules((current) => [...current, rule]);
    return rule;
  }

  function remove(key: string) {
    setIssues(NO_ISSUES);
    setRules((current) => current.filter((rule) => rule.key !== key || rule.builtin));
  }

  function move(key: string, by: -1 | 1) {
    setIssues(NO_ISSUES);
    setRules((current) =>
      moveRule(
        current,
        current.findIndex((rule) => rule.key === key),
        by,
      ),
    );
  }

  return {
    load,
    reload,
    rules,
    revision,
    publishedVersion,
    dirty: load.status === "ready" && fingerprint(rules) !== saved,
    busy,
    issues,
    failure,
    stale,
    change,
    add,
    remove,
    move,
    save,
    publish,
    discard: reload,
  };
}
