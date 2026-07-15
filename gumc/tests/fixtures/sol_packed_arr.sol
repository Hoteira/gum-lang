// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;
contract P {
    uint8[] a;
    uint8[4] f;
    uint256 sentinel;
    function push(uint8 v) external { a.push(v); }
    function pop() external { a.pop(); }
    function setf(uint256 i, uint8 v) external { f[i] = v; }
    function getf(uint256 i) external view returns (uint8) { return f[i]; }
    function get(uint256 i) external view returns (uint8) { return a[i]; }
    function len() external view returns (uint256) { return a.length; }
    function sum() external view returns (uint256) {
        uint256 s = 0;
        for (uint256 i = 0; i < a.length; i++) { s += a[i]; }
        return s;
    }
}
