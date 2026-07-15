// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;

// A faithful 1:1 Solidity twin of token.gum, matching what gum *actually
// emits* (same fairness rules as amm_equivalent.sol):
//
//   * State vars are NOT `public`, gum generates no getters.
//   * `require(cond, "msg")` carries the same revert string as the gum side's
//     assert(cond, "msg"), gum lowers it to the standard Error(string)
//     payload, so the reason bytes are compared too.
//   * `initialize` has no re-entry guard, gum's `once` currently emits none.
//   * `.saturate()` is saturating addition (caps at type(uint256).max on
//     overflow), NOT Solidity's default reverting `+`. Replicated via
//     unchecked + cap so the behavior matches, not just the shape.
//   * The low-level `call` in airdropAndNotify IGNORES success, gum computes
//     the call's ok flag but never reverts on it (fire-and-forget). Matched.
//   * `calculateBonus` is `external`: gum assigns it a selector and a
//     dispatcher case even though it's a non-`global` fn, so it is part of
//     gum's external surface and must be here too for an equal comparison.
//
// Addresses use `address` (160-bit). gum now masks Account params to 160 bits
// at the calldata boundary too, so this is genuine parity rather than a gap.

contract Token {
    address owner;
    uint256 totalSupply;
    mapping(address => uint256) balances;

    // Match token.gum's events exactly (same names, same param types/order),
    // so topic0 hashes are identical. Transfer's topic0 is the canonical
    // ERC20 0xddf252ad... that wallets and indexers recognize.
    event Transfer(address indexed from, address indexed to, uint256 value);
    event Mint(address indexed to, uint256 value);

    function initialize(uint256 initialSupply) external {
        address sender = msg.sender;
        owner = sender;
        totalSupply = initialSupply;
        balances[sender] = initialSupply;
    }

    function transfer(address to, uint256 amount) external {
        _transfer(to, amount);
    }

    function mint(address to, uint256 amount) external {
        require(msg.sender == owner, "Only owner can mint");
        balances[to] = _satAdd(balances[to], amount);
        totalSupply = _satAdd(totalSupply, amount);

        emit Mint(to, amount);
    }

    function calculateBonus(uint256 amount) external pure returns (uint256) {
        return amount / 100;
    }

    function airdropAndNotify(address to, uint256 amount, address targetContract) external {
        _transfer(to, amount);

        bytes memory payload = abi.encodePacked(amount);
        // Fire-and-forget: gum's bare `call` statement does not check success.
        (bool ok, ) = targetContract.call(payload);
        ok; // silence unused-return warning; intentionally not reverted on
    }

    function _transfer(address to, uint256 amount) private {
        address sender = msg.sender;
        require(balances[sender] >= amount, "Insufficient balance");
        balances[sender] = balances[sender] - amount;
        balances[to] = _satAdd(balances[to], amount);

        // In _transfer (not transfer) so airdropAndNotify, which calls it,
        // also emits, exactly as gum's transfer_impl does for both callers.
        emit Transfer(sender, to, amount);
    }

    function _satAdd(uint256 a, uint256 b) private pure returns (uint256 r) {
        unchecked {
            r = a + b;
            if (r < a) r = type(uint256).max;
        }
    }
}
