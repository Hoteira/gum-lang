// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Ledger {
    uint256 total_;
    function credit(uint256 v) public { total_ += v; }
    function cap() public pure virtual returns (uint256) { return 100; }
}
contract Owned is Ledger {
    address owner_;
    function claim() public { owner_ = msg.sender; }
}
contract Bank is Owned {
    uint256 fee_;
    function cap() public pure override returns (uint256) { return 250; }
    function cap_of() external pure returns (uint256) { return cap(); }
    function set_fee(uint256 v) external { fee_ = v; }
    function total() external view returns (uint256) { return total_; }
    function owner() external view returns (address) { return owner_; }
    function fee() external view returns (uint256) { return fee_; }
}
