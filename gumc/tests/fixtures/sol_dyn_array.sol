// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract C {
    uint256[] items;
    function push_val(uint256 v) external { items.push(v); }
    function set_at(uint256 i, uint256 v) external { items[i] = v; }
    function get(uint256 i) external view returns (uint256) { return items[i]; }
    function len() external view returns (uint256) { return items.length; }
    function pop_val() external { items.pop(); }
}
