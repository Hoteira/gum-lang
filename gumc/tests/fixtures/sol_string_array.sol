// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;
contract Strs {
    function echo(string[] calldata xs) external pure returns (string[] memory) { return xs; }
    function first(string[] calldata xs) external pure returns (string memory) { return xs[0]; }
    function alen(string[] calldata xs) external pure returns (uint256) { return xs.length; }
    function plen(string[] calldata xs) external pure returns (uint256) { return bytes(xs[0]).length; }
}
