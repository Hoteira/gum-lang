// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract BadChild {
    constructor() { revert("child ctor failed"); }
}
