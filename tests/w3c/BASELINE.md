# W3C Baseline

This snapshot establishes parser and corpus coverage only. It does not report
SCXML conformance results and does not execute fixtures through the legacy
single-path runtime.

| Measurement | Count |
|---|---:|
| Manifest assertions | 200 |
| Start resources | 202 |
| Dependency resources | 9 |
| XML resources parsed | 206 / 206 |
| Mandatory assertions | 166 |
| Optional assertions | 34 |
| Manual assertions | 9 |
| Executed assertions | 0 |

The parser retains all W3C `conf:*` markup as foreign namespaced syntax. The
test-profile inventory currently recognizes all 11 conformance elements and 75
conformance attributes used by this pinned corpus. Translation into ordinary
SCXML and the `w3c-test` datamodel is the next harness step; unknown constructs
will be rejected rather than ignored.

Runtime pass/fail reporting begins only after documents compile into the real
SCXML configuration and transition-set interpreter. No fixture is lowered into
`StateTreeBuilder` as a temporary compatibility path.
