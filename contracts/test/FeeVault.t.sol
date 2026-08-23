// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {IFeeVault, IMockUSDC, ISwapRouter} from "../src/interfaces.sol";

/// A router that pays out at a fixed rate, so the vault's arithmetic is what
/// is under test rather than a pool's.
contract MockRouter is ISwapRouter {
    IMockUSDC public immutable tokenOut;
    uint256 public rate = 2; // two project tokens per USDC micro

    constructor(IMockUSDC out) {
        tokenOut = out;
    }

    function exactInputSingle(ExactInputSingleParams calldata p) external override returns (uint256) {
        IMockUSDC(p.tokenIn).transferFrom(msg.sender, address(this), p.amountIn);
        uint256 out = p.amountIn * rate;
        require(out >= p.amountOutMinimum, "slippage");
        tokenOut.mint(p.recipient, out);
        return out;
    }
}

contract FeeVaultTest is Test {
    IMockUSDC internal usdc;
    IMockUSDC internal project;
    MockRouter internal router;
    IFeeVault internal vault;

    address internal sink = address(0xDEAD);
    uint64 internal constant EPOCH = 7 days;

    function setUp() public {
        usdc = IMockUSDC(deployCode("src/MockUSDC.vy"));
        project = IMockUSDC(deployCode("src/MockUSDC.vy"));
        router = new MockRouter(project);
        vault = IFeeVault(
            deployCode(
                "src/FeeVault.vy",
                abi.encode(address(usdc), address(project), address(router), uint24(3000), sink, EPOCH)
            )
        );
        usdc.mint(address(vault), 1_000e6); // a week of fees
    }

    function test_a_buyback_spends_the_epochs_fees_and_sends_the_token_to_the_sink() public {
        vm.warp(block.timestamp + EPOCH);
        uint256 caller = 1_000e6 * 25 / 10_000;

        vault.buyback(0);

        assertEq(project.balanceOf(sink), (1_000e6 - caller) * 2, "bought and forwarded");
        assertEq(usdc.balanceOf(address(this)), caller, "whoever paid the gas is paid for it");
        assertEq(vault.pending(), 0, "nothing left sitting");
    }

    function test_it_cannot_be_called_twice_in_one_epoch() public {
        vm.warp(block.timestamp + EPOCH);
        vault.buyback(0);

        // Batching is the point: a buyback per payment would be a sandwich
        // opportunity attached to every job.
        vm.expectRevert("TooSoon");
        vault.buyback(0);
    }

    function test_a_caller_can_refuse_a_bad_fill() public {
        vm.warp(block.timestamp + EPOCH);
        vm.expectRevert("slippage");
        vault.buyback(type(uint256).max);

        // And the epoch is not consumed by an attempt that failed.
        vault.buyback(0);
        assertGt(project.balanceOf(sink), 0);
    }
}
