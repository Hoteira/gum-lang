// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract C {
    function hash_str(string calldata s) external pure returns (uint256) {
        return uint256(keccak256(bytes(s)));
    }
    function rec(uint256 h, uint8 v, uint256 r, uint256 s) external pure returns (address) {
        return ecrecover(bytes32(h), v, bytes32(r), bytes32(s));
    }
}
