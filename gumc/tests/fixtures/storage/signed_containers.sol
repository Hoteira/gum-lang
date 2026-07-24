// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;
contract C {
  mapping(uint256 => int32) m;
  int32[] a;
  struct P { int32 s; } P p;
  function setm(uint256 k, int32 v) external { m[k]=v; }
  function addm(uint256 k, int32 v) external { m[k]=m[k]+v; }
  function pusha(int32 v) external { a.push(v); }
  function adda(uint256 i, int32 v) external { a[i]=a[i]+v; }
  function setp(int32 v) external { p.s=v; }
  function addp(int32 v) external { p.s=p.s+v; }
}
