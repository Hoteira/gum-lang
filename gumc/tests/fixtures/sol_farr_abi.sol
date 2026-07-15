// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract C {
    uint256 total;
    function sum3(uint256[3] calldata xs) external pure returns (uint256) {
        uint256 s = 0;
        for (uint256 i = 0; i < 3; i++) { s += xs[i]; }
        return s;
    }
    function echo3(uint256[3] calldata xs) external pure returns (uint256[3] memory) { return xs; }
    function sum3_8(uint8[3] calldata xs) external pure returns (uint256) {
        uint256 s = 0;
        for (uint256 i = 0; i < 3; i++) { s += xs[i]; }
        return s;
    }
    function echo3_8(uint8[3] calldata xs) external pure returns (uint8[3] memory) { return xs; }
    function mixed(uint256 a, uint8[3] calldata xs, uint256 b) external pure returns (uint256) {
        uint256 s = a + b;
        for (uint256 i = 0; i < 3; i++) { s += xs[i]; }
        return s;
    }
}
