// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

struct P {
    uint128 a;
    uint256 b;
}

contract Child {
    uint256 public stored;

    constructor(P memory p) {
        stored = p.b;
    }

    function get_b() external view returns (uint256) {
        return stored;
    }
}

contract Parent {
    function make_and_read(P calldata p) external returns (uint256) {
        Child c = new Child(p);
        return c.get_b();
    }
}
