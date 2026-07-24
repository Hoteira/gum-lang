// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract C {
    enum Status { Active, Paused, Closed }

    event Changed(uint8 s);

    Status state;
    uint256 afterEnum;
    mapping(address => Status) perUser;

    function set_state(Status s) external { state = s; }
    function get_state() external view returns (Status) { return state; }
    function set_after(uint256 v) external { afterEnum = v; }
    function get_after() external view returns (uint256) { return afterEnum; }
    function set_user(address k, Status s) external { perUser[k] = s; }
    function get_user(address k) external view returns (Status) { return perUser[k]; }
    function emit_it(Status s) external { emit Changed(uint8(s)); }
}
