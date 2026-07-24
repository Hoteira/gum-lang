// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Meta {
    string name;
    uint256 supply;
    function set_name(string calldata n) external { name = n; }
    function name_of() external view returns (string memory) { return name; }
    function set_supply(uint256 s) external { supply = s; }
    function supply_of() external view returns (uint256) { return supply; }
}
