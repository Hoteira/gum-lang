// SPDX-License-Identifier: MIT
pragma solidity ^0.8.12;
contract App {
    function cat(string calldata a, string calldata b) external pure returns (string memory) {
        return string.concat(a, b);
    }
    function same(string calldata a, string calldata b) external pure returns (bool) {
        return keccak256(bytes(a)) == keccak256(bytes(b));
    }
    function at(string calldata a, uint256 i) external pure returns (uint8) {
        return uint8(bytes(a)[i]);
    }
    function cut(bytes calldata a, uint256 s, uint256 e) external pure returns (bytes memory) {
        return a[s:e];
    }
    function len(string calldata a) external pure returns (uint256) {
        return bytes(a).length;
    }
}
