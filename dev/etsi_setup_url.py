"""Robot listener: fixtures outside the view (surfaces-conformance.md 2.3, T-3501).

A TP's setup and teardown, its suite's too, reach the plain root given as the
listener's first argument; its body reaches the URL under test (`url` and
`temporal_api_url` of variables.py, a view's NGSI-LD root). A view refuses
the writes most setups make, so without this the run would judge the
fixtures, not the TP.

The optional second argument is a file the listener appends one JSON line to
for every request a TP's body sends (test, method, URL with its query,
header names, and the JSON body it carries, up to 64 KiB): dev/etsi-view-diff.py
reads it to tell a refusal the view owes from a deviation, a hidden type or
Attribute named in the body included (T-3787).

    robot --listener "dev/etsi_setup_url.py;http://localhost:9090/ngsi-ld/v1;requests.jsonl" ...
"""

import json

from robot.libraries.BuiltIn import BuiltIn

NAMES = ("${url}", "${temporal_api_url}")
FIXTURE = ("SETUP", "TEARDOWN")
METHODS = ("GET", "POST", "PUT", "PATCH", "DELETE")
BODY = ("json", "data")
MAX_BODY = 64 * 1024


class etsi_setup_url:  # Robot takes the class named as its module
    ROBOT_LISTENER_API_VERSION = 3

    def __init__(self, setup_url, record=None):
        self.setup_url = setup_url.rstrip("/")
        self.record = record
        self.under_test = None
        self.depth = 0
        self.test = None

    def _point(self, at_setup):
        builtin = BuiltIn()
        if self.under_test is None:
            self.under_test = {n: builtin.get_variable_value(n) for n in NAMES}
        for name in NAMES:
            builtin.set_global_variable(name, self.setup_url if at_setup else self.under_test[name])

    def start_test(self, data, result):
        # the key dev/etsi-report.py's tp-status.csv gives the same TP
        source = str(getattr(data, "source", None) or data.parent.source or "")
        self.test = f"{source.split('ngsi-ld-test-suite/')[-1]}::{data.name}"

    def end_test(self, data, result):
        self.test = None

    def start_keyword(self, data, result):
        if result.type in FIXTURE:
            if self.depth == 0:
                self._point(at_setup=True)
            self.depth += 1
        elif self.record and self.test and self.depth == 0 and data.name in METHODS \
                and getattr(result, "owner", None) == "RequestsLibrary":
            self._log(data.name, data.args)

    def end_keyword(self, data, result):
        if result.type in FIXTURE:
            self.depth -= 1
            if self.depth == 0:
                self._point(at_setup=False)

    def _log(self, method, args):
        """The request as RequestsLibrary sends it: `url=` or the first
        positional argument, `headers=` resolved in the caller's scope."""
        builtin = BuiltIn()
        named = {}
        positional = []
        for arg in args:
            key, sep, value = str(arg).partition("=")
            if sep and key in ("url", "headers", "params") + BODY:
                named[key] = value
            else:
                positional.append(str(arg))
        url = named.get("url") or (positional[0] if positional else "")
        url = str(builtin.replace_variables(url))
        headers = builtin.replace_variables(named["headers"]) if "headers" in named else {}
        params = builtin.replace_variables(named["params"]) if "params" in named else ""
        if isinstance(params, dict):
            params = "&".join(f"{k}={v}" for k, v in params.items())
        if params:
            url += ("&" if "?" in url else "?") + str(params)
        line = {"test": self.test, "method": method, "url": url,
                "headers": sorted(str(k).lower() for k in headers) if isinstance(headers, dict) else []}
        body = next((named[k] for k in BODY if k in named), None)
        if body is not None:
            line["body"] = self._json(builtin.replace_variables(body))
        with open(self.record, "a", encoding="utf-8") as fh:
            fh.write(json.dumps(line) + "\n")

    @staticmethod
    def _json(value):
        """The body as JSON: a dict or list as Robot holds it, a string parsed,
        anything larger than MAX_BODY or not JSON left out (None)."""
        if isinstance(value, (bytes, bytearray)):
            value = value.decode("utf-8", "replace")
        if isinstance(value, str):
            if len(value) > MAX_BODY:
                return None
            try:
                return json.loads(value)
            except ValueError:
                return None
        if isinstance(value, (dict, list)):
            text = json.dumps(value, default=str)
            return value if len(text) <= MAX_BODY else None
        return None
