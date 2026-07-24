// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

struct P {
    uint128 a;
    uint256 b;
    uint8 c;
    address d;
    bool e;
}

contract S {
    function fa(P calldata p) external pure returns (uint128) {
        return p.a;
    }

    function fb(P calldata p) external pure returns (uint256) {
        return p.b;
    }

    function fc(P calldata p) external pure returns (uint8) {
        return p.c;
    }

    function fd(P calldata p) external pure returns (address) {
        return p.d;
    }

    function fe(P calldata p) external pure returns (bool) {
        return p.e;
    }

    function echo(P calldata p) external pure returns (P memory) {
        return p;
    }

    function mix(uint256 x, P calldata p, uint256 y) external pure returns (uint256) {
        return x + p.b + y;
    }
}
