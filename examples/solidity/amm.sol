// SPDX-License-Identifier: MIT
pragma solidity 0.8.36;

// A faithful 1:1 Solidity twin of amm.gum, written to match what gum
// *actually emits*, not idiomatic Solidity, so the bytecode-size comparison
// is fair rather than flattering to either side. Specifically:
//
//   * State vars are NOT `public`: gum generates no getters for its class
//     fields, so adding Solidity getters would inflate this side unfairly.
//   * `require(cond, "msg")` carries the SAME revert string as the gum side's
//     assert(cond, "msg"): gum lowers it to the standard Error(string) payload,
//     so the differential test compares the reason bytes too.
//   * `initialize` has no re-entry guard: gum's `once` modifier currently
//     emits no guard in the generated Yul, so neither does this.
//
// One difference can't be erased without abandoning idiomatic Solidity:
// gum stores Account as a full uint256 and passes it to CALL unmasked,
// whereas Solidity's `address` type masks to 160 bits at trust boundaries.
// That masking is genuine work solc must do and gum skips, it is part of
// the honest picture, so this uses `address` as a normal dev would.

interface IERC20 {
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}

contract AMM {
    address tokenA;
    address tokenB;
    uint256 reserveA;
    uint256 reserveB;
    uint256 totalShares;
    mapping(address => uint256) shares;
    // Mirrors gum's `once` one-shot guard: a dedicated flag slot, set 0->1 on the
    // first call, so the cold SSTORE cost is the same on both sides.
    bool initialized;

    event LiquidityAdded(address indexed sender, uint256 amountA, uint256 amountB, uint256 sharesMinted);
    event SwapExecuted(address indexed sender, address indexed tokenIn, uint256 amountIn, address tokenOut, uint256 amountOut);

    function initialize(address _tokenA, address _tokenB) external {
        require(!initialized, "already initialized");
        initialized = true;
        tokenA = _tokenA;
        tokenB = _tokenB;
    }

    // Named to match gum's `add_liquidity` so the differential harness can
    // drive both compilers' output with one calldata (selectors match). The
    // name has zero bytecode-size effect, the selector is 4 bytes regardless.
    function add_liquidity(uint256 amountA, uint256 amountB) external {
        address sender = msg.sender;

        require(IERC20(tokenA).transferFrom(sender, address(this), amountA), "Transfer A failed");
        require(IERC20(tokenB).transferFrom(sender, address(this), amountB), "Transfer B failed");

        reserveA += amountA;
        reserveB += amountB;

        uint256 sharesToMint = amountA;
        shares[sender] += sharesToMint;
        totalShares += sharesToMint;

        emit LiquidityAdded(sender, amountA, amountB, sharesToMint);
    }

    function swap(address tokenIn, uint256 amountIn) external {
        address sender = msg.sender;

        require(tokenIn == tokenA || tokenIn == tokenB, "Invalid token");

        uint256 reserveIn;
        uint256 reserveOut;
        address tokenOut;

        if (tokenIn == tokenA) {
            reserveIn = reserveA;
            reserveOut = reserveB;
            tokenOut = tokenB;
        } else {
            reserveIn = reserveB;
            reserveOut = reserveA;
            tokenOut = tokenA;
        }

        uint256 amountOut = (reserveOut * amountIn) / (reserveIn + amountIn);

        require(amountOut > 0, "Insufficient output amount");
        require(IERC20(tokenIn).transferFrom(sender, address(this), amountIn), "Transfer in failed");
        require(IERC20(tokenOut).transfer(sender, amountOut), "Transfer out failed");

        if (tokenIn == tokenA) {
            reserveA += amountIn;
            reserveB -= amountOut;
        } else {
            reserveB += amountIn;
            reserveA -= amountOut;
        }

        emit SwapExecuted(sender, tokenIn, amountIn, tokenOut, amountOut);
    }
}
