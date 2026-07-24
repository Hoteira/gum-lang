// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract V {
    uint256 received;
    uint256 fell_;
    receive() external payable { received += msg.value; }
    fallback() external payable { fell_ += 1; }
    function total() external view returns (uint256) { return received; }
    function fell() external view returns (uint256) { return fell_; }
}
