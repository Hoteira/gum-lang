// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;
contract C {
  uint256 a;
  function seta(uint256 v) external { a = v; }
  function a_lit() external view returns (uint256) { return a * 2; }
  function a_var(uint256 b) external view returns (uint256) { return a * b; }
  function u8_lit_r(uint8 v) external pure returns (uint8) { return v * 2; }
  function u8_lit_l(uint8 v) external pure returns (uint8) { return 2 * v; }
  function u8_var(uint8 v, uint8 w) external pure returns (uint8) { return v * w; }
  function i8_lit_r(int8 v) external pure returns (int8) { return v * 2; }
  function i8_lit_l(int8 v) external pure returns (int8) { return 2 * v; }
  function i8_var(int8 v, int8 w) external pure returns (int8) { return v * w; }
}
