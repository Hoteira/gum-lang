// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Packed {
    uint128 a;
    uint256 big;
    uint128 b;
    function setall(uint128 x, uint256 y, uint128 z) external { a = x; big = y; b = z; }
}
