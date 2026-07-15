// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract C {
    enum Status { Active, Paused, Closed }

    function after_enum(Status s, uint256 x) external pure returns (uint256) {
        return x;
    }

    function between(uint256 a, Status s, uint256 b) external pure returns (uint256) {
        return a + b;
    }

    function tag(Status s) external pure returns (uint256) {
        if (s == Status.Active) return 10;
        if (s == Status.Paused) return 20;
        return 30;
    }

    function echo(Status s) external pure returns (Status) {
        return s;
    }

    function pick(uint256 x) external pure returns (Status) {
        if (x == 0) return Status.Active;
        return Status.Closed;
    }
}
