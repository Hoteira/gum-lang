// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;
contract Lists {
    mapping(address => uint256[]) items;
    function add(address who, uint256 v) external { items[who].push(v); }
    function get(address who, uint256 i) external view returns (uint256) { return items[who][i]; }
    function set(address who, uint256 i, uint256 v) external { items[who][i] = v; }
    function size(address who) external view returns (uint256) { return items[who].length; }
    function drop_last(address who) external { items[who].pop(); }
    function clear(address who) external { delete items[who]; }
}
