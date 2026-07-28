#!/usr/bin/env python3
"""Create or update the StandX maker OpenObserve alerts.

Two push-based safety nets, provisioned together:

1. **Deadman** — fires when the ``standx_maker`` stream has seen no
   ``action='cycle_summary'`` event for the deadman window (~3 minutes). This
   is the net for issue #220: a silent death (SIGKILL / OOM / panic / host
   down) stops emitting cycle summaries, so the deadman trips even though the
   process itself can no longer notify anyone.
2. **Critical risk** — fires when any row with ``severity='critical'`` lands in
   the stream: stop-loss, account floor, accounting invariant, cleanup with
   residual orders, residual-position handoff/unknown. The deadman only covers
   "the process died"; this covers "the process is alive and something went
   wrong". Until this alert existed, the only immediate channel for those
   events was the maker's own webhook POST, which is not retried — a slow or
   broken endpoint meant nobody heard about it.

   The condition is deliberately ``severity='critical'`` alone rather than a
   per-action list: it is a single condition (the same shape the deadman
   already proves works on this deployment) and it picks up any future critical
   event without another migration.

   Known limitation: the alert text says *that* a critical row arrived within
   the window, not which one — only ``{stream_name}`` / ``{alert_name}`` /
   ``{org_name}`` are verified-substituting variables on this build. Use the
   dashboard's "Rejections & Error Signals" panel for the row itself.

Environment (shares the dashboard script's OpenObserve variables):

- ``OPENOBSERVE_URL``           default ``http://127.0.0.1:5080``
- ``OPENOBSERVE_ORG``           default ``default``
- ``OPENOBSERVE_STREAM``        default ``standx_maker``
- ``OPENOBSERVE_USER`` / ``OPENOBSERVE_PASSWORD``   required (Basic auth)
- ``OPENOBSERVE_ALERT_WEBHOOK`` required; Feishu (Lark) custom-bot webhook the
  alert POSTs to. The template body is Feishu msg_type=text, so a Slack or
  generic ``{"text": ...}`` endpoint will reject it.
- ``OPENOBSERVE_ALERT_MINUTES`` default ``3``; deadman window in minutes
- ``OPENOBSERVE_CRITICAL_SILENCE_MINUTES`` default ``5``; how long the critical
  alert stays silent after firing. Critical events arrive in bursts on a
  fail-safe shutdown (floor breach -> residual handoff -> stopped), so a short
  silence keeps one incident to one notification.
"""

from __future__ import annotations

import base64
import json
import os
import re
import sys
from typing import Any
from urllib import error, parse, request


DEADMAN_ALERT_NAME = "standx_maker_deadman"
DEADMAN_TEMPLATE_NAME = "standx_maker_deadman_template"
DEADMAN_DESTINATION_NAME = "standx_maker_deadman_webhook"
CRITICAL_ALERT_NAME = "standx_maker_critical_risk"
CRITICAL_TEMPLATE_NAME = "standx_maker_critical_risk_template"
CRITICAL_DESTINATION_NAME = "standx_maker_critical_risk_webhook"
NAME_RE = re.compile(r"^[A-Za-z0-9_]+$")

# Feishu (Lark) custom-bot text payload. OpenObserve substitutes the {var}
# placeholders at send time; the JSON structure braces are left untouched
# because only recognized variable names are replaced. Feishu requires the
# msg_type/content envelope rather than a bare {"text": ...} body.
_DEADMAN_TEXT = (
    "\U0001f6d1 StandX maker DEADMAN: no cycle_summary in the "
    "{stream_name} stream for the deadman window. The maker may have "
    "died silently (SIGKILL/OOM/panic/host down) and could be leaving "
    "resting orders on the venue. Alert: {alert_name} org: {org_name}"
)
DEADMAN_TEMPLATE_BODY = json.dumps(
    {"msg_type": "text", "content": {"text": _DEADMAN_TEXT}}
)

_CRITICAL_TEXT = (
    "\U0001f6a8 StandX maker CRITICAL: a severity=critical event landed in "
    "the {stream_name} stream (stop-loss, account floor, accounting "
    "invariant, cleanup residual orders, or residual-position handoff). The "
    "process may be shutting down and may be leaving a position or orders on "
    "the venue. Check the dashboard 'Rejections & Error Signals' panel for the "
    "row. Alert: {alert_name} org: {org_name}"
)
CRITICAL_TEMPLATE_BODY = json.dumps(
    {"msg_type": "text", "content": {"text": _CRITICAL_TEXT}}
)


def build_deadman_alert(stream: str, minutes: int) -> dict[str, Any]:
    """Scheduled alert that trips when fewer than one cycle_summary row is
    seen within the deadman window."""
    return {
        "name": DEADMAN_ALERT_NAME,
        "stream_type": "logs",
        "stream_name": stream,
        "is_real_time": False,
        "query_condition": {
            "type": "custom",
            "conditions": [
                {
                    "column": "action",
                    "operator": "=",
                    "value": "cycle_summary",
                }
            ],
            "sql": "",
            "promql": "",
            "promql_condition": None,
            "aggregation": None,
            "vrl_function": None,
            "search_event_type": None,
        },
        "trigger_condition": {
            # Count matching rows over the last `minutes`; fire when there are
            # none (< 1). Re-evaluate every minute and silence repeats for the
            # window so a prolonged outage does not spam the channel.
            "period": minutes,
            "operator": "<",
            "threshold": 1,
            "frequency": 1,
            "frequency_type": "minutes",
            "silence": minutes,
            "timezone": "UTC",
        },
        "destinations": [DEADMAN_DESTINATION_NAME],
        "context_attributes": {},
        "row_template": "",
        "description": (
            "Deadman: fires when the maker stops emitting cycle_summary "
            "events, i.e. it likely died without running cleanup (issue #220)."
        ),
        "enabled": True,
    }


def build_critical_alert(stream: str, silence_minutes: int) -> dict[str, Any]:
    """Scheduled alert that trips as soon as a severity=critical row arrives.

    Evaluated every minute over the last minute: any match fires. The silence
    window collapses a burst (a fail-safe shutdown emits several critical rows
    in a row) into one notification.
    """
    return {
        "name": CRITICAL_ALERT_NAME,
        "stream_type": "logs",
        "stream_name": stream,
        "is_real_time": False,
        "query_condition": {
            "type": "custom",
            "conditions": [
                {
                    "column": "severity",
                    "operator": "=",
                    "value": "critical",
                }
            ],
            "sql": "",
            "promql": "",
            "promql_condition": None,
            "aggregation": None,
            "vrl_function": None,
            "search_event_type": None,
        },
        "trigger_condition": {
            "period": 1,
            "operator": ">=",
            "threshold": 1,
            "frequency": 1,
            "frequency_type": "minutes",
            "silence": silence_minutes,
            "timezone": "UTC",
        },
        "destinations": [CRITICAL_DESTINATION_NAME],
        "context_attributes": {},
        # Best-effort row detail: harmless if this build does not substitute it.
        "row_template": "{kind}/{event}: {message}",
        "description": (
            "Critical risk: fires on any severity=critical row (stop-loss, "
            "account floor, accounting invariant, cleanup residual orders, "
            "residual-position handoff). Covers 'alive but broken', which the "
            "deadman does not."
        ),
        "enabled": True,
    }


class OpenObserve:
    def __init__(self) -> None:
        self.url = os.getenv("OPENOBSERVE_URL", "http://127.0.0.1:5080").rstrip("/")
        self.org = os.getenv("OPENOBSERVE_ORG", "default")
        self.stream = os.getenv("OPENOBSERVE_STREAM", "standx_maker")
        self.webhook = os.getenv("OPENOBSERVE_ALERT_WEBHOOK", "")
        try:
            self.minutes = int(os.getenv("OPENOBSERVE_ALERT_MINUTES", "3"))
        except ValueError as exc:
            raise RuntimeError("OPENOBSERVE_ALERT_MINUTES must be an integer") from exc
        try:
            self.critical_silence = int(
                os.getenv("OPENOBSERVE_CRITICAL_SILENCE_MINUTES", "5")
            )
        except ValueError as exc:
            raise RuntimeError(
                "OPENOBSERVE_CRITICAL_SILENCE_MINUTES must be an integer"
            ) from exc
        username = os.getenv("OPENOBSERVE_USER", "")
        password = os.getenv("OPENOBSERVE_PASSWORD", "")
        if not username or not password:
            raise RuntimeError("OPENOBSERVE_USER and OPENOBSERVE_PASSWORD are required")
        if not self.webhook:
            raise RuntimeError("OPENOBSERVE_ALERT_WEBHOOK is required")
        if self.minutes < 1:
            raise RuntimeError("OPENOBSERVE_ALERT_MINUTES must be >= 1")
        if self.critical_silence < 1:
            raise RuntimeError("OPENOBSERVE_CRITICAL_SILENCE_MINUTES must be >= 1")
        if not NAME_RE.fullmatch(self.org) or not NAME_RE.fullmatch(self.stream):
            raise RuntimeError(
                "OpenObserve org and stream may contain only letters, digits, and underscore"
            )
        credential = base64.b64encode(f"{username}:{password}".encode()).decode()
        self.headers = {
            "Authorization": f"Basic {credential}",
            "Content-Type": "application/json",
            "Accept": "application/json",
            "User-Agent": "standx-openobserve-alerts/1",
        }

    def json_request(
        self, method: str, path: str, payload: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        data = None if payload is None else json.dumps(payload).encode()
        req = request.Request(self.url + path, data=data, headers=self.headers, method=method)
        try:
            with request.urlopen(req, timeout=30) as response:
                body = response.read()
                return json.loads(body) if body else {}
        except error.HTTPError as exc:
            detail = exc.read(4096).decode(errors="replace")
            raise RuntimeError(
                f"OpenObserve {method} {path} returned HTTP {exc.code}: {detail}"
            ) from exc
        except (error.URLError, TimeoutError) as exc:
            raise RuntimeError(f"OpenObserve {method} {path} failed: {exc}") from exc

    def _org(self) -> str:
        return parse.quote(self.org, safe="")

    def _exists(self, path: str, name: str, key: str = "list") -> bool:
        listing = self.json_request("GET", path)
        items = listing.get(key, listing) if isinstance(listing, dict) else listing
        if not isinstance(items, list):
            return False
        return any(isinstance(item, dict) and item.get("name") == name for item in items)

    def upsert_template(self, name: str, body: str) -> str:
        base = f"/api/{self._org()}/alerts/templates"
        payload = {"name": name, "body": body, "isDefault": False}
        if self._exists(base, name):
            self.json_request("PUT", f"{base}/{parse.quote(name, safe='')}", payload)
            return "updated"
        self.json_request("POST", base, payload)
        return "created"

    def upsert_destination(self, name: str, template_name: str) -> str:
        base = f"/api/{self._org()}/alerts/destinations"
        payload = {
            "name": name,
            "url": self.webhook,
            "method": "post",
            "skip_tls_verify": False,
            "template": template_name,
            "headers": {},
        }
        if self._exists(base, name):
            self.json_request("PUT", f"{base}/{parse.quote(name, safe='')}", payload)
            return "updated"
        self.json_request("POST", base, payload)
        return "created"

    def _find_alert_id(self, base: str, name: str) -> str | None:
        listing = self.json_request("GET", base)
        items = listing.get("list", []) if isinstance(listing, dict) else []
        if not isinstance(items, list):
            return None
        return next(
            (
                item.get("alert_id")
                for item in items
                if isinstance(item, dict) and item.get("name") == name
            ),
            None,
        )

    def upsert_alert(self, name: str, alert: dict[str, Any]) -> str:
        # Alerts moved to the v2 API (OpenObserve >= ~0.14): org-scoped, keyed
        # by alert_id rather than name, with stream_name as a body field
        # instead of a path segment. The old stream-scoped v1 path
        # (/api/{org}/{stream}/alerts) 404s on current OpenObserve builds.
        base = f"/api/v2/{self._org()}/alerts"
        alert_id = self._find_alert_id(base, name)
        if alert_id:
            self.json_request("PUT", f"{base}/{parse.quote(alert_id, safe='')}", alert)
            return "updated"
        self.json_request("POST", base, alert)
        return "created"


def main() -> int:
    client = OpenObserve()
    # Each alert owns its own template + destination: OpenObserve binds a
    # template to a destination, and the two alerts need different texts.
    specs = [
        (
            DEADMAN_ALERT_NAME,
            DEADMAN_TEMPLATE_NAME,
            DEADMAN_TEMPLATE_BODY,
            DEADMAN_DESTINATION_NAME,
            build_deadman_alert(client.stream, client.minutes),
        ),
        (
            CRITICAL_ALERT_NAME,
            CRITICAL_TEMPLATE_NAME,
            CRITICAL_TEMPLATE_BODY,
            CRITICAL_DESTINATION_NAME,
            build_critical_alert(client.stream, client.critical_silence),
        ),
    ]
    provisioned = []
    for alert_name, template_name, template_body, destination_name, alert in specs:
        template_action = client.upsert_template(template_name, template_body)
        destination_action = client.upsert_destination(destination_name, template_name)
        alert_action = client.upsert_alert(alert_name, alert)
        provisioned.append(
            {
                "template": {"name": template_name, "action": template_action},
                "destination": {"name": destination_name, "action": destination_action},
                "alert": {"name": alert_name, "action": alert_action},
            }
        )
    print(
        json.dumps(
            {
                "stream": client.stream,
                "deadman_minutes": client.minutes,
                "critical_silence_minutes": client.critical_silence,
                "provisioned": provisioned,
            },
            indent=2,
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RuntimeError as exc:
        print(f"openobserve alerts error: {exc}", file=sys.stderr)
        raise SystemExit(1)
