// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Echo {
    function echo(string calldata s) external pure returns (string memory) { return s; }
    function get_len(string calldata s) external pure returns (uint256) { return bytes(s).length; }
}
