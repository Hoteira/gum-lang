// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

// Reaches a target with STATICCALL, which is what solc emits when one contract
// calls another's `view` function. Anything that writes state, including a
// TSTORE reentrancy guard, reverts inside it.
contract Prober {
    function probe(address t, bytes calldata data) external view returns (bool ok) {
        (ok, ) = t.staticcall(data);
    }

    // Diagnostics: what came back, and how much of it.
    function probe_full(address t, bytes calldata data)
        external
        view
        returns (bool ok, uint256 len, bytes memory ret)
    {
        (ok, ret) = t.staticcall(data);
        len = ret.length;
    }

    function echo(bytes calldata data) external pure returns (bytes memory) {
        return data;
    }
}
