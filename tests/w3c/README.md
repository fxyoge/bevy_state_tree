# W3C SCXML Implementation Report Suite

`vendor/` contains an immutable, byte-for-byte snapshot of the resources linked
by the W3C SCXML 1.0 Implementation Report manifest, plus the two official
datamodel transforms. Do not edit, normalize, fix, or regenerate individual
files under `vendor/`.

Implementation failures must be fixed in the parser, compiler, interpreter, or
test-profile translator. The W3C fixtures are the authority even when a test
exposes behavior that the library does not yet support.

Run `scripts/update-w3c-suite.sh` only when intentionally reviewing a new
upstream snapshot. It downloads into a staging directory and regenerates
`vendor.lock.toml`; review every changed fixture and hash before committing.
Normal tests are offline and must never update the snapshot.

The pinned manifest is an exact archived response from the official W3C URL.
W3C rate-limits bulk downloads, so the importer acquires manifest-listed
payload bytes from a pinned commit of the established USCXML mirror. The lock
records both sources. Sample fixtures and both official transforms were
verified byte-for-byte against W3C responses before accepting the mirror.

The suite is distributed under the W3C 3-Clause BSD Test Suite License. See
`LICENSE-W3C-BSD.txt`. Its implementation report explicitly states that the
suite demonstrates implementability and interoperability rather than providing
complete SCXML conformance certification.

Generated SCXML and result reports belong under `target/`, never in `vendor/`.
