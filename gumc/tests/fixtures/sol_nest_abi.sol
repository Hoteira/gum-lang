// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract N {
    struct P {
        uint128 a;
        uint256 b;
    }

    function rows(uint256[][] calldata xs) external pure returns (uint256) {
        return xs.length;
    }

    function row_len(uint256[][] calldata xs, uint256 i) external pure returns (uint256) {
        return xs[i].length;
    }

    function total(uint256[][] calldata xs) external pure returns (uint256) {
        uint256 t = 0;
        for (uint256 i = 0; i < xs.length; i++) {
            for (uint256 j = 0; j < xs[i].length; j++) {
                t += xs[i][j];
            }
        }
        return t;
    }

    function at(uint256[][] calldata xs, uint256 i, uint256 j) external pure returns (uint256) {
        return xs[i][j];
    }

    function echo(uint256[][] calldata xs) external pure returns (uint256[][] memory) {
        return xs;
    }

    function deep_at(uint256[][][] calldata xs, uint256 i, uint256 j, uint256 k) external pure returns (uint256) {
        return xs[i][j][k];
    }

    function pair_sum(uint256[][2] calldata xs) external pure returns (uint256) {
        uint256 t = 0;
        for (uint256 i = 0; i < 2; i++) {
            for (uint256 j = 0; j < xs[i].length; j++) {
                t += xs[i][j];
            }
        }
        return t;
    }

    function fixed_rows(uint256[3][] calldata xs, uint256 i, uint256 j) external pure returns (uint256) {
        return xs[i][j];
    }

    function struct_pair_b(P[2] calldata xs, uint256 i) external pure returns (uint256) {
        return xs[i].b;
    }

    function struct_grid_b(P[][] calldata xs, uint256 i, uint256 j) external pure returns (uint256) {
        return xs[i][j].b;
    }
}
