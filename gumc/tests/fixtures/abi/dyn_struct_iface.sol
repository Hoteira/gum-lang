// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;
struct Meta { uint256 id; string name; }
interface IStore { function store(Meta calldata m) external returns (Meta memory); }
contract Target { function store(Meta calldata m) external pure returns (Meta memory) { return m; } }
contract Caller { function call_it(address t, uint256 i, string calldata n) external returns (Meta memory) { Meta memory m = Meta(i, n); return IStore(t).store(m); } }
