// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Holder {
    uint256 total_;
    uint256 count_;
    constructor(uint256[] memory xs, uint256 extra) {
        uint256 s = extra;
        for (uint256 i = 0; i < xs.length; i++) { s += xs[i]; }
        total_ = s;
        count_ = xs.length;
    }
    function total() external view returns (uint256) { return total_; }
    function count() external view returns (uint256) { return count_; }
}
contract Maker {
    address last_;
    function make(uint256[] calldata xs, uint256 extra) external returns (address) {
        last_ = address(new Holder(xs, extra));
        return last_;
    }
}
