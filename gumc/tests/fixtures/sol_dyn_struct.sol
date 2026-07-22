// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;
contract C {
    struct Meta { uint256 id; string name; }
    function echo(Meta calldata m) external pure returns (Meta memory) { return m; }
}
