// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract D {
    struct Stake { uint256 amount; uint256 since; }
    uint256 n;
    string name;
    uint256[] arr;
    uint8[4] fixed_;
    mapping(address => uint256) bal;
    mapping(address => Stake) stakes;
    uint8 packed_a;
    uint8 packed_b_;

    function fill(address who, string calldata s) external {
        n = 42;
        packed_a = 7;
        packed_b_ = 9;
        name = s;
        arr.push(1); arr.push(2); arr.push(3);
        fixed_[0] = 1; fixed_[1] = 2;
        bal[who] = 500;
        stakes[who].amount = 10;
        stakes[who].since = 20;
    }
    function wipe(address who) external {
        delete n;
        delete packed_a;
        delete name;
        delete arr;
        delete fixed_;
        delete bal[who];
        delete stakes[who];
    }
    function len() external view returns (uint256) { return arr.length; }
    function packed_b() external view returns (uint8) { return packed_b_; }
}
