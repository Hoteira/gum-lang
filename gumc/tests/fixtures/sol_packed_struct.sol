// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract C {
    struct Pair { uint128 lo; uint128 hi; }
    mapping(address => Pair) m;
    function setp(address k, uint128 a, uint128 b) external { m[k].lo = a; m[k].hi = b; }
    function getlo(address k) external view returns (uint128) { return m[k].lo; }
    function gethi(address k) external view returns (uint128) { return m[k].hi; }
}
