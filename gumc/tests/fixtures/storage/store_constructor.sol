// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Store {
    uint256 value;
    constructor(uint256 x) { value = x; }
    function get() external view returns (uint256) { return value; }
}
