// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

struct P {
    uint128 a;
    uint256 b;
    address c;
}

contract S {
    function count(P[] calldata xs) external pure returns (uint256) {
        return xs.length;
    }

    function sum_b(P[] calldata xs) external pure returns (uint256) {
        uint256 t = 0;
        for (uint256 i = 0; i < xs.length; i++) {
            t += xs[i].b;
        }
        return t;
    }

    function at_a(P[] calldata xs, uint256 i) external pure returns (uint128) {
        return xs[i].a;
    }

    function at_c(P[] calldata xs, uint256 i) external pure returns (address) {
        return xs[i].c;
    }

    function echo(P[] calldata xs) external pure returns (P[] memory) {
        return xs;
    }

    function bump(P[] calldata xs, uint256 i, uint128 v) external pure returns (uint128) {
        P[] memory m = xs;
        m[i].a = v;
        return m[i].a;
    }
}
