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
    address internal stranger = address(0xBEEF);
    address internal ops = address(0xA11);
    uint64 internal constant EPOCH = 7 days;

    function setUp() public {
        usdc = IMockUSDC(deployCode("src/MockUSDC.vy"));
        project = IMockUSDC(deployCode("src/MockUSDC.vy"));
        router = new MockRouter(project);
        vault = IFeeVault(deployCode("src/FeeVault.vy", abi.encode(address(usdc))));
        usdc.mint(address(vault), 1_000e6); // a week of fees
    }

    function _enableBuyToken() internal {
        vault.setSwap(address(project), address(router), 3000, sink, EPOCH);
        vault.setBuyToken(true);
        vm.warp(block.timestamp + EPOCH);
    }

    function test_the_deployer_is_admin_and_buybacks_start_off() public view {
        assertEq(vault.admin(), address(this));
        assertFalse(vault.buyToken());
        assertEq(vault.projectToken(), address(0));
        assertEq(vault.router(), address(0));
        assertEq(vault.sink(), address(0));
        assertEq(vault.epoch(), 0);
    }

    function test_admin_can_withdraw_collected_usdc() public {
        uint256 pulled = vault.withdraw(ops, 0);
        assertEq(pulled, 1_000e6);
        assertEq(usdc.balanceOf(ops), 1_000e6);
        assertEq(vault.pending(), 0);
    }

    function test_admin_can_withdraw_a_partial_amount() public {
        vault.withdraw(ops, 250e6);
        assertEq(usdc.balanceOf(ops), 250e6);
        assertEq(vault.pending(), 750e6);
    }

    function test_a_stranger_cannot_withdraw() public {
        vm.prank(stranger);
        vm.expectRevert("OnlyAdmin");
        vault.withdraw(stranger, 0);
    }

    function test_buyback_is_off_until_the_switch() public {
        vm.expectRevert("BuyTokenOff");
        vault.buyback(0);
    }

    function test_a_stranger_cannot_flip_the_switch() public {
        vm.prank(stranger);
        vm.expectRevert("OnlyAdmin");
        vault.setBuyToken(true);
    }

    function test_a_buyback_spends_the_epochs_fees_and_sends_the_token_to_the_sink() public {
        _enableBuyToken();

        vault.buyback(0);

        assertEq(project.balanceOf(sink), 1_000e6 * 2, "bought and forwarded");
        assertEq(usdc.balanceOf(address(this)), 0, "no caller skim");
        assertEq(vault.pending(), 0, "nothing left sitting");
    }

    function test_a_stranger_cannot_buyback() public {
        _enableBuyToken();
        vm.prank(stranger);
        vm.expectRevert("OnlyAdmin");
        vault.buyback(0);
    }

    function test_it_cannot_be_called_twice_in_one_epoch() public {
        _enableBuyToken();
        vault.buyback(0);

        // Batching is the point: a buyback per payment would be a sandwich
        // opportunity attached to every job.
        vm.expectRevert("TooSoon");
        vault.buyback(0);
    }

    function test_a_caller_can_refuse_a_bad_fill() public {
        _enableBuyToken();
        vm.expectRevert("slippage");
        vault.buyback(type(uint256).max);

        // And the epoch is not consumed by an attempt that failed.
        vault.buyback(0);
        assertGt(project.balanceOf(sink), 0);
    }

    function test_admin_can_hand_the_keys_to_someone_else() public {
        vault.setAdmin(ops);
        assertEq(vault.admin(), ops);

        vm.expectRevert("OnlyAdmin");
        vault.withdraw(address(this), 0);

        vm.prank(ops);
        vault.withdraw(ops, 0);
        assertEq(usdc.balanceOf(ops), 1_000e6);
    }

    function test_turning_buyToken_on_needs_a_swap_path() public {
        IFeeVault blank = IFeeVault(deployCode("src/FeeVault.vy", abi.encode(address(usdc))));
        vm.expectRevert("SwapUnset");
        blank.setBuyToken(true);

        blank.setSwap(address(project), address(router), 3000, sink, EPOCH);
        blank.setBuyToken(true);
        assertTrue(blank.buyToken());
    }
}
