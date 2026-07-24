// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract C {
    uint256 total;
    function sum(uint256[] calldata xs) external pure returns (uint256) {
        uint256 s = 0;
        for (uint256 i = 0; i < xs.length; i++) { s += xs[i]; }
        return s;
    }
    function echo(uint256[] calldata xs) external pure returns (uint256[] memory) { return xs; }
    function at(uint256[] calldata xs, uint256 i) external pure returns (uint256) { return xs[i]; }
    function len_of(uint256[] calldata xs) external pure returns (uint256) { return xs.length; }
    function sum8(uint8[] calldata xs) external pure returns (uint256) {
        uint256 s = 0;
        for (uint256 i = 0; i < xs.length; i++) { s += xs[i]; }
        return s;
    }
    function echo8(uint8[] calldata xs) external pure returns (uint8[] memory) { return xs; }
    function two(uint256[] calldata a, uint256 n, uint8[] calldata b) external pure returns (uint256) {
        uint256 s = n;
        for (uint256 i = 0; i < a.length; i++) { s += a[i]; }
        for (uint256 i = 0; i < b.length; i++) { s += b[i]; }
        return s;
    }
}
