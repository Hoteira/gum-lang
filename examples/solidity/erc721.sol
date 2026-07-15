// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;

// 1:1 Solidity twin of erc721.gum. Storage order: owners(0), balances(1),
// approvals(2), operator_approvals(3). Snake_case names to share calldata.
contract Erc721 {
    mapping(uint256 => address) owners;
    mapping(address => uint256) balances;
    mapping(uint256 => address) approvals;
    mapping(address => mapping(address => bool)) operator_approvals;

    function mint(address to, uint256 id) external {
        require(owners[id] == address(0), "already minted");
        owners[id] = to;
        balances[to] = balances[to] + 1;
    }

    function owner_of(uint256 id) external view returns (address) {
        return owners[id];
    }

    function balance_of(address who) external view returns (uint256) {
        return balances[who];
    }

    function approve(address to, uint256 id) external {
        approvals[id] = to;
    }

    function get_approved(uint256 id) external view returns (address) {
        return approvals[id];
    }

    function set_approval_for_all(address op, bool ok) external {
        operator_approvals[msg.sender][op] = ok;
    }

    function is_approved_for_all(address owner, address op) external view returns (bool) {
        return operator_approvals[owner][op];
    }

    function transfer_from(address from, address to, uint256 id) external {
        require(owners[id] == from, "not owner");
        owners[id] = to;
        balances[from] = balances[from] - 1;
        balances[to] = balances[to] + 1;
        approvals[id] = address(0);
    }
}
