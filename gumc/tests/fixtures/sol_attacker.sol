// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Attacker {
    address public target;
    uint256 public depth;
    function setTarget(address t) external { target = t; }
    function ping() external returns (bool) {
        depth++;
        if (depth < 2) {
            (bool ok, ) = target.call(abi.encodeWithSignature("poke(address)", address(this)));
            require(ok, "reentry blocked");
        }
        return true;
    }
}
