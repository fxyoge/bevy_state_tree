default:
  @just --list

test: \
  _test-macros \
  _test-lib

[working-directory: 'macros']
_test-macros:
  cargo test

_test-lib:
  cargo test

bench: \
  _bench-event-storage \
  _bench-execution \
  _bench-allocation

_bench-event-storage:
  cargo bench --bench event_storage

_bench-execution:
  cargo bench --bench execution

_bench-allocation:
  cargo bench --bench allocation
