// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract Reg {
    mapping(address => mapping(address => uint256)) m;
    function setv(address a, address b, uint256 v) external { m[a][b] = v; }
    function getv(address a, address b) external view returns (uint256) { return m[a][b]; }
    function incv(address a, address b, uint256 d) external { m[a][b] = m[a][b] + d; }
}
