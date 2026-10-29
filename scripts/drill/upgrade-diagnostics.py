#!/usr/bin/env python3
"""Redact the drill's diagnostic stream before it reaches CI output."""

import re
import sys


def redact(text):
    text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", text)
    # Database error details can contain entire rows, not just identifiers.
    text = re.sub(r"(?im)\bDETAIL:.*$", "DETAIL: [redacted]", text)
    text = re.sub(r"(?i)\bsetup token\s+\S+", "setup token [redacted]", text)
    text = re.sub(
        r"(?i)(\b(?:userId|anonymousId|external:user_id|messageId|person_id|"
        r"workspace_id|trace_id|job_id|cause_id|token|password|authorization)"
        r"[\"']?\s*[:=]\s*)(\"(?:\\.|[^\"\\])*\"|'[^']*'|[^\s,;}]+)",
        r"\1[redacted]", text,
    )
    text = re.sub(r"[\w.!#$%&'*+/=?^`{|}~-]+@[\w.-]+", "[redacted-email]", text)
    text = re.sub(r"\b[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}\b",
                  "[redacted-id]", text)
    text = re.sub(r"\b(?:drill-user|person|event)-[0-9]+\b", "[redacted-id]", text)
    text = re.sub(r"(?<!\w)\+?(?:\(?[0-9][ ()\t.-]*){6,}[0-9](?!\w)",
                  "[redacted-phone]", text)
    text = re.sub(r"(?<![\w])[A-Za-z0-9_+/-]{24,}={0,2}", "[redacted-token]", text)
    return text


def self_test():
    for value in (
        "owner@example.com", "+45 20 30 40 50", "(415) 555-0123", "415.555.0123",
        "01234567-89ab-cdef-0123-456789abcdef", "drill-user-0", "person-2999", "event-42",
        "aB1_" * 10,
    ):
        assert value not in redact(value), "identifier survived redaction"
    for line, value in (
        ('userId="0"', "0"), ('"userId": "2999"', "2999"), ("external:user_id=17", "17"),
        ("setup token secret", "secret"), ("password=secret", "secret"),
        ("DETAIL: Failing row contains (private value).", "private value"),
    ):
        assert value not in redact(line), "field value survived redaction"
    safe = "stage=population_ingest expected=3000 last=2999 pending=500 reason=apply_failed"
    assert redact(safe) == safe, "redaction removed diagnostic counts"
    assert redact("state=running attempts=2 duration_ms=1000") == \
        "state=running attempts=2 duration_ms=1000"
    print("upgrade diagnostics self-test OK (identifiers redacted, counts retained)")


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
    elif not sys.argv[1:]:
        for line in sys.stdin:
            sys.stdout.write(redact(line))
    else:
        sys.exit("usage: upgrade-diagnostics.py [--self-test]")
