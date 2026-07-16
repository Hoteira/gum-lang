# tools/

Third-party binaries live here. They are not committed.

## solc

gum emits Yul; `solc` assembles it into EVM bytecode. It is also used to build
the Solidity reference contracts that the differential tests compare against.

The compiler takes `--solc <path>`, or finds `solc` on your PATH. The tests look
in this order:

1. `$SOLC`
2. `tools/solc` or `tools/solc.exe`
3. `solc` on PATH

Drop a binary here and it wins over PATH, which keeps your runs on the version
this repo was verified against. Releases:
<https://github.com/ethereum/solidity/releases>

Without solc, `gumc` still emits Yul and ABI JSON, and the tests that need
bytecode skip rather than fail. CI sets `GUM_REQUIRE_SOLC=1`, which turns that
skip into a failure: 68 of the execution tests need solc, and a silent skip
there would be a green run that checked nothing.

CI pins solc to the version in `.github/workflows/ci.yml`. The size table in the
README is measured against it, so a version bump is a deliberate commit rather
than a surprise.
