// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Store {
    uint256 value;
    function set(uint256 x) external { value = x; }
    function add(uint256 y) external { value = value + y; }
    function get() external view returns (uint256) { return value; }
}
