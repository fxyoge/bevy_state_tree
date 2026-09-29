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
  _bench-execution \
  _bench-allocation

_bench-execution:
  cargo bench --bench execution

_bench-allocation:
  cargo bench --bench allocation
