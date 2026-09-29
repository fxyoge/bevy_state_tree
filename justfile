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

bench:
  cargo bench --bench execution
