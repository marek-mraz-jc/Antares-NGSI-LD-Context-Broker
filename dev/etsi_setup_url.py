"""Robot listener: fixtures outside the view (surfaces-conformance.md 2.3, T-3501).

A TP's setup and teardown, its suite's too, reach the plain root given as the
listener's argument; its body reaches the URL under test (`url` and
`temporal_api_url` of variables.py, a view's NGSI-LD root). A view refuses
the writes most setups make, so without this the run would judge the
fixtures, not the TP.

    robot --listener "dev/etsi_setup_url.py;http://localhost:9090/ngsi-ld/v1" ...
"""

from robot.libraries.BuiltIn import BuiltIn

NAMES = ("${url}", "${temporal_api_url}")
FIXTURE = ("SETUP", "TEARDOWN")


class etsi_setup_url:  # Robot takes the class named as its module
    ROBOT_LISTENER_API_VERSION = 3

    def __init__(self, setup_url):
        self.setup_url = setup_url.rstrip("/")
        self.under_test = None
        self.depth = 0

    def _point(self, at_setup):
        builtin = BuiltIn()
        if self.under_test is None:
            self.under_test = {n: builtin.get_variable_value(n) for n in NAMES}
        for name in NAMES:
            builtin.set_global_variable(name, self.setup_url if at_setup else self.under_test[name])

    def start_keyword(self, data, result):
        if result.type in FIXTURE:
            if self.depth == 0:
                self._point(at_setup=True)
            self.depth += 1

    def end_keyword(self, data, result):
        if result.type in FIXTURE:
            self.depth -= 1
            if self.depth == 0:
                self._point(at_setup=False)
