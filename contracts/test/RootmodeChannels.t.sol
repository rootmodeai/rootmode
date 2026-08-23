// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {IRootmodeChannels, IMockUSDC} from "../src/interfaces.sol";

contract RootmodeChannelsTest is Test {
    IRootmodeChannels internal channels;
    IMockUSDC internal usdc;

    address internal vault = address(0xFEE);
    address internal workerPayout = address(0xB0B);

    uint256 internal clientKey = 0xA11CE;
    address internal client;

    bytes32 internal channelId = keccak256("client:worker:session");

    function setUp() public {
        usdc = IMockUSDC(deployCode("src/MockUSDC.vy"));
        channels = IRootmodeChannels(deployCode("src/RootmodeChannels.vy", abi.encode(address(usdc), vault)));
        client = vm.addr(clientKey);

        usdc.mint(client, 100e6); // $100
        vm.prank(client);
        usdc.approve(address(channels), type(uint256).max);
        vm.prank(client);
        channels.deposit(client, 100e6);
    }

    // ------------------------------------------------------------- signing

    function _sign(uint256 key, bytes32 structHash) internal view returns (bytes memory) {
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", channels.domainSeparator(), structHash));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        return abi.encodePacked(r, s, v);
    }

    function _reserveSig(uint256 key, uint256 maxAmount, uint64 deadline) internal view returns (bytes memory) {
        return _sign(
            key,
            keccak256(
                abi.encode(
                    keccak256(
                        "ReserveAuth(bytes32 channelId,address client,address workerPayout,uint256 maxAmount,uint256 deadline)"
                    ),
                    channelId,
                    client,
                    workerPayout,
                    maxAmount,
                    uint256(deadline)
                )
            )
        );
    }

    function _spendSig(uint256 key, uint256 cumulative, bytes32 metadataHash) internal view returns (bytes memory) {
        return _sign(
            key,
            keccak256(
                abi.encode(
                    keccak256("SpendingAuth(bytes32 channelId,address client,uint256 cumulative,bytes32 metadataHash)"),
                    channelId,
                    client,
                    cumulative,
                    metadataHash
                )
            )
        );
    }

    function _open(uint256 maxAmount, uint64 deadline) internal {
        channels.reserve(channelId, client, workerPayout, maxAmount, deadline, _reserveSig(clientKey, maxAmount, deadline));
    }

    // --------------------------------------------------------------- tests

    function test_a_redemption_pays_the_worker_and_takes_ten_percent() public {
        _open(20e6, uint64(block.timestamp + 1 days));

        // $2.73 authorised for work already delivered.
        channels.redeem(channelId, 2_730_000, keccak256("job"), _spendSig(clientKey, 2_730_000, keccak256("job")));

        assertEq(usdc.balanceOf(workerPayout), 2_457_000, "90% to the worker");
        assertEq(usdc.balanceOf(vault), 273_000, "10% to the vault");
        assertEq(usdc.balanceOf(workerPayout) + usdc.balanceOf(vault), 2_730_000, "nothing stuck");
    }

    function test_each_redemption_pays_only_the_difference() public {
        _open(20e6, uint64(block.timestamp + 1 days));
        channels.redeem(channelId, 1e6, keccak256("a"), _spendSig(clientKey, 1e6, keccak256("a")));
        channels.redeem(channelId, 3e6, keccak256("b"), _spendSig(clientKey, 3e6, keccak256("b")));

        // $3 total authorised, not $4: cumulative, not per-job.
        assertEq(usdc.balanceOf(workerPayout), 2_700_000);
        assertEq(usdc.balanceOf(vault), 300_000);
    }

    function test_an_old_authorisation_cannot_be_replayed() public {
        _open(20e6, uint64(block.timestamp + 1 days));
        channels.redeem(channelId, 3e6, keccak256("b"), _spendSig(clientKey, 3e6, keccak256("b")));

        // Built before the expectation, because signing reads the domain
        // separator and `expectRevert` binds to the very next call.
        bytes memory stale = _spendSig(clientKey, 1e6, keccak256("a"));

        // The worker keeps only the newest for a reason: the earlier ones are
        // worth nothing, and presenting one is an error rather than a payment.
        vm.expectRevert("NotMonotonic");
        channels.redeem(channelId, 1e6, keccak256("a"), stale);
    }

    function test_a_worker_cannot_be_paid_more_than_was_reserved() public {
        _open(5e6, uint64(block.timestamp + 1 days));

        bytes memory sig = _spendSig(clientKey, 6e6, keccak256("job"));

        // Even with a real signature from the real client: the reservation is
        // the ceiling, and the rest of the balance is not exposed to it.
        vm.expectRevert("OverReserved");
        channels.redeem(channelId, 6e6, keccak256("job"), sig);
    }

    function test_somebody_elses_signature_pays_nothing() public {
        _open(20e6, uint64(block.timestamp + 1 days));
        bytes memory forged = _spendSig(0xBAD, 1e6, keccak256("job"));

        vm.expectRevert("BadSignature");
        channels.redeem(channelId, 1e6, keccak256("job"), forged);
    }

    function test_reserved_money_cannot_be_withdrawn_from_under_a_worker() public {
        _open(20e6, uint64(block.timestamp + 1 days));

        assertEq(channels.balanceOf(client), 80e6, "the reservation left the spendable balance");
        vm.prank(client);
        vm.expectRevert("NotEnough");
        channels.withdraw(100e6);
    }

    function test_after_the_deadline_the_unearned_remainder_goes_back() public {
        uint64 deadline = uint64(block.timestamp + 1 days);
        _open(20e6, deadline);
        channels.redeem(channelId, 5e6, keccak256("job"), _spendSig(clientKey, 5e6, keccak256("job")));

        vm.expectRevert("StillOpen");
        channels.close(channelId);

        vm.warp(deadline + 1);
        channels.close(channelId);

        // $20 reserved, $5 earned: the other $15 is the client's again, and
        // the $5 already paid is not clawed back.
        assertEq(channels.balanceOf(client), 95e6);
        vm.prank(client);
        channels.withdraw(95e6);
        assertEq(usdc.balanceOf(client), 95e6);
    }

    function test_a_closed_channel_pays_no_more() public {
        uint64 deadline = uint64(block.timestamp + 1 days);
        _open(20e6, deadline);
        vm.warp(deadline + 1);
        channels.close(channelId);
        bytes memory sig = _spendSig(clientKey, 1e6, keccak256("job"));

        vm.expectRevert("OverReserved");
        channels.redeem(channelId, 1e6, keccak256("job"), sig);
    }

    function test_a_reservation_can_be_topped_up_without_double_charging() public {
        uint64 deadline = uint64(block.timestamp + 1 days);
        _open(20e6, deadline);
        _open(30e6, deadline);

        // $30 reserved in total, not $50.
        assertEq(channels.balanceOf(client), 70e6);
    }

    function testFuzz_a_worker_never_receives_more_than_was_authorised(uint96 authorised) public {
        vm.assume(authorised > 0 && authorised <= 20e6);
        _open(20e6, uint64(block.timestamp + 1 days));
        channels.redeem(channelId, authorised, keccak256("job"), _spendSig(clientKey, authorised, keccak256("job")));

        assertLe(usdc.balanceOf(workerPayout), authorised);
        assertEq(usdc.balanceOf(workerPayout) + usdc.balanceOf(vault), authorised, "no dust is created or lost");
        // Rounding favours the worker, and never the other way.
        assertGe(usdc.balanceOf(workerPayout) * 10_000, uint256(authorised) * 9_000);
    }
}
