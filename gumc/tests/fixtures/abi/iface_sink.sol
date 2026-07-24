// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

struct P {
    uint128 a;
    uint256 b;
}

contract Sink {
    function take(P calldata p) external pure returns (uint256) {
        return uint256(p.a) + p.b;
    }

    function take_str(string calldata s) external pure returns (uint256) {
        return bytes(s).length;
    }

    function take_arr(uint256[] calldata xs) external pure returns (uint256) {
        uint256 t = 0;
        for (uint256 i = 0; i < xs.length; i++) {
            t += xs[i];
        }
        return t;
    }

    function take_starr(P[] calldata xs) external pure returns (uint256) {
        uint256 t = 0;
        for (uint256 i = 0; i < xs.length; i++) {
            t += uint256(xs[i].a) + xs[i].b;
        }
        return t;
    }

    function mk(uint256 x) external pure returns (P memory) {
        return P(7, x);
    }

    function name() external pure returns (string memory) {
        return "gumball";
    }

    function nums() external pure returns (uint256[] memory) {
        uint256[] memory r = new uint256[](3);
        r[0] = 5;
        r[1] = 6;
        r[2] = 7;
        return r;
    }

    function pairs() external pure returns (P[] memory) {
        P[] memory r = new P[](2);
        r[0] = P(1, 111);
        r[1] = P(2, 222);
        return r;
    }
}
