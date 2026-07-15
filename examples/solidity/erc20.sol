// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;

// 1:1 Solidity twin of erc20.gum. Same storage order (total_supply, balances,
// allowances) and same snake_case function names so selectors match and one
// calldata drives both compilers in the differential test. require() carries
// no string (gum lowers assert messages to blank reverts); state vars are not
// public (gum emits no getters).
contract Erc20 {
    uint256 total_supply;
    mapping(address => uint256) balances;
    mapping(address => mapping(address => uint256)) allowances;

    function init(uint256 supply) external {
        address s = msg.sender;
        total_supply = supply;
        balances[s] = supply;
    }

    function balance_of(address who) external view returns (uint256) {
        return balances[who];
    }

    function approve(address spender, uint256 amount) external {
        allowances[msg.sender][spender] = amount;
    }

    function allowance(address owner, address spender) external view returns (uint256) {
        return allowances[owner][spender];
    }

    function transfer(address to, uint256 amount) external {
        address from = msg.sender;
        require(balances[from] >= amount, "insufficient balance");
        balances[from] = balances[from] - amount;
        balances[to] = balances[to] + amount;
    }

    function transfer_from(address from, address to, uint256 amount) external {
        address spender = msg.sender;
        require(allowances[from][spender] >= amount, "insufficient allowance");
        require(balances[from] >= amount, "insufficient balance");
        allowances[from][spender] = allowances[from][spender] - amount;
        balances[from] = balances[from] - amount;
        balances[to] = balances[to] + amount;
    }
}
