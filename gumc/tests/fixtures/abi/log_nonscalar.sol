// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract L {
    struct P {
        uint128 a;
        uint256 b;
    }

    event Arr(uint256[] xs);
    event Str(string s);
    event Tup(P p);
    event Grid(uint256[][] g);
    event Mixed(address indexed who, uint256 n, uint256[] xs, string s);

    function arr(uint256[] calldata xs) external {
        emit Arr(xs);
    }

    function str(string calldata s) external {
        emit Str(s);
    }

    function tup(P calldata p) external {
        emit Tup(p);
    }

    function grid(uint256[][] calldata g) external {
        emit Grid(g);
    }

    function mixed(address who, uint256 n, uint256[] calldata xs, string calldata s) external {
        emit Mixed(who, n, xs, s);
    }
}
