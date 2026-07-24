// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract C {
    uint256[3] items;
    uint256 total;
    function setit(uint256 i, uint256 v) external { items[i] = v; total = total + v; }
    function getit(uint256 i) external view returns (uint256) { return items[i]; }
}
