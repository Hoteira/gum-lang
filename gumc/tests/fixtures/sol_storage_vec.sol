// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract S {
    uint256[] items;
    uint256 sentinel;
    function add(uint256 v) external { items.push(v); }
    function drop() external { items.pop(); }
    function get(uint256 i) external view returns (uint256) { return items[i]; }
    function at(uint256 i) external view returns (uint256) { return items[i]; }
    function len() external view returns (uint256) { return items.length; }
    function length() external view returns (uint256) { return items.length; }
    function wipe() external { delete items; }
    function set_sentinel(uint256 v) external { sentinel = v; }
}
