// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract C {
    function who() external payable returns (address) {
        return msg.sender;
    }

    function amount() external payable returns (uint256) {
        return msg.value;
    }

    function me() external view returns (address) {
        return address(this);
    }

    function when() external view returns (uint256) {
        return block.timestamp;
    }

    function height() external view returns (uint256) {
        return block.number;
    }
}
