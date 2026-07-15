// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Bench {
    mapping(address => uint256) m;
    function sum5(address k) external view returns (uint256) {
        return m[k] + m[k] + m[k] + m[k] + m[k];
    }
}
