// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Batcher {
    address public target;
    function setTarget(address t) external { target = t; }
    function ping() external pure returns (bool) { return true; }
    function twice() external {
        (bool a, ) = target.call(abi.encodeWithSignature("poke(address)", address(this)));
        require(a, "first poke failed");
        (bool b, ) = target.call(abi.encodeWithSignature("poke(address)", address(this)));
        require(b, "second poke failed");
    }
}
