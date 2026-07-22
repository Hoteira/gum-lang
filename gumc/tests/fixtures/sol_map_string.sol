// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;
contract Names {
    mapping(address => string) names;
    function set(address who, string calldata name) external { names[who] = name; }
    function get(address who) external view returns (string memory) { return names[who]; }
    function clear(address who) external { delete names[who]; }
}
