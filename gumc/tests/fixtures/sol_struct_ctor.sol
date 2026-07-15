// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

struct P {
    uint128 a;
    uint256 b;
    address d;
}

contract C {
    uint256 public sa;
    uint256 public sb;
    address public sd;

    constructor(P memory p) {
        sa = p.a;
        sb = p.b;
        sd = p.d;
    }

    function get_a() external view returns (uint256) {
        return sa;
    }

    function get_b() external view returns (uint256) {
        return sb;
    }

    function get_d() external view returns (address) {
        return sd;
    }
}
