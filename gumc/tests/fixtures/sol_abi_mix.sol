// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract AbiMix {
    function pick(uint256 which, string calldata a, string calldata b) external pure returns (string memory) {
        if (which == 0) { return a; }
        return b;
    }
    function total_len(string calldata a, string calldata b) external pure returns (uint256) {
        return bytes(a).length + bytes(b).length;
    }
}
