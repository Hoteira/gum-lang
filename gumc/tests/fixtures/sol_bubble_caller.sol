// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
interface IToken { function transfer(address to, uint256 amount) external returns (bool); }
contract Caller {
    function send(address token, address to, uint256 amount) external returns (bool) {
        return IToken(token).transfer(to, amount);
    }
}
