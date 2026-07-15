# tools/

Third-party binaries live here. They are not committed.

## solc

gum emits Yul; `solc` assembles it into EVM bytecode. It is also used to build
the Solidity reference contracts that the differential tests compare against.

Put a `solc` binary at `tools/solc.exe` (or `tools/solc` on Linux and macOS),
or pass `--solc /path/to/solc`. Releases:
<https://github.com/ethereum/solidity/releases>

Without it, `gumc` still emits Yul and ABI JSON, and the tests that need
bytecode skip rather than fail.
