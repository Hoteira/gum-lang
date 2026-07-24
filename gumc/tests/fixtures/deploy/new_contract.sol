// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Child {
    uint256 v;
    address parent;
    constructor(uint256 x) { v = x; parent = msg.sender; }
    function get() external view returns (uint256) { return v; }
    function parent_of() external view returns (address) { return parent; }
}
contract Factory {
    uint256 count_;
    address last_;
    function make(uint256 x) external returns (address) {
        count_ += 1;
        Child c = new Child(x);
        last_ = address(c);
        return address(c);
    }
    function count() external view returns (uint256) { return count_; }
    function last() external view returns (address) { return last_; }
}
