// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Mock {
    fallback() external {
        assembly { mstore(0, 1) return(0, 32) }
    }
}
