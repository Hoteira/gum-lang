// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
error InsufficientBalance(uint256 available, uint256 required);
contract Tok {
    function transfer(address to, uint256 amount) external pure returns (bool) {
        if (amount == 1) revert("ERC20: transfer amount exceeds balance");
        if (amount == 2) revert InsufficientBalance(7, 9);
        return true;
    }
}
