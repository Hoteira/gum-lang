// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Token {
    string name_;
    string symbol_;
    uint256 supply_;
    constructor(string memory n, uint256 s, string memory sym) {
        name_ = n; supply_ = s; symbol_ = sym;
    }
    function name() external view returns (string memory) { return name_; }
    function symbol() external view returns (string memory) { return symbol_; }
    function supply() external view returns (uint256) { return supply_; }
}
contract Deployer {
    address last_;
    function make(string calldata n, uint256 s, string calldata sym) external returns (address) {
        last_ = address(new Token(n, s, sym));
        return last_;
    }
}
