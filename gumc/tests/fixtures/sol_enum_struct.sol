// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;
contract C {
    enum E { A, B, C }
    struct S { uint256 x; E tag; }
    function get_x(S calldata s) external pure returns (uint256) { return s.x; }
    function get_tag(S calldata s) external pure returns (uint256) {
        if (s.tag == E.A) return 10;
        if (s.tag == E.B) return 20;
        if (s.tag == E.C) return 30;
        return 99;
    }
}
