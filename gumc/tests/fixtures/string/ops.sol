// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract StrOps {
    function cat(string calldata a, string calldata b) external pure returns (string memory) {
        return string(abi.encodePacked(a, b));
    }
    function same(string calldata a, string calldata b) external pure returns (bool) {
        return keccak256(bytes(a)) == keccak256(bytes(b));
    }
    function differs(string calldata a, string calldata b) external pure returns (bool) {
        return keccak256(bytes(a)) != keccak256(bytes(b));
    }
    function at(string calldata a, uint256 i) external pure returns (uint8) {
        return uint8(bytes(a)[i]);
    }
    function cut(string calldata a, uint256 s, uint256 e) external pure returns (string memory) {
        return string(bytes(a)[s:e]);
    }
}
