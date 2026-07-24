// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Vault {
    uint256 total;
    function deposit() external payable { total = total + msg.value; }
    function poke() external { total = total + 1; }
    function total_of() external view returns (uint256) { return total; }
}
