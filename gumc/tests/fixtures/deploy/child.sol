// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Child {
    uint256 v;
    constructor() payable { v = 42; }
    function get() external view returns (uint256) { return v; }
}
