// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;

// 1:1 Solidity twin of vault.gum. Storage: total (slot 0), stakes (slot 1);
// each Stake occupies two consecutive slots (amount, since) at its mapping
// hash. Same function names so one calldata drives both compilers.
contract Vault {
    struct Stake {
        uint256 amount;
        uint256 since;
    }
    uint256 total;
    mapping(address => Stake) stakes;

    function deposit(uint256 amt, uint256 t) external {
        stakes[msg.sender].amount = stakes[msg.sender].amount + amt;
        stakes[msg.sender].since = t;
        total = total + amt;
    }

    function amount_of(address who) external view returns (uint256) {
        return stakes[who].amount;
    }

    function since_of(address who) external view returns (uint256) {
        return stakes[who].since;
    }

    function withdraw(uint256 amt) external {
        require(stakes[msg.sender].amount >= amt, "insufficient stake");
        stakes[msg.sender].amount = stakes[msg.sender].amount - amt;
        total = total - amt;
    }
}
