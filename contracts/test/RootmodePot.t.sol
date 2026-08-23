// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {IRootmodePot, IMockUSDC} from "../src/interfaces.sol";

contract RootmodePotTest is Test {
    IRootmodePot internal pot;
    IMockUSDC internal usdc;

    address internal vault = address(0xFEE);
    address internal worker = address(0xB0B);

    uint256 internal clientKey = 0xA11CE;
    uint256 internal appKey = 0xA22;
    address internal client;
    address internal app;

    uint256 internal constant MAX_JOB = 500_000; // $0.50
    uint256 internal constant MAX_DAY = 20e6; // $20
    uint64 internal constant GRACE = 15 minutes;

    function setUp() public {
        usdc = IMockUSDC(deployCode("src/MockUSDC.vy"));
        pot = IRootmodePot(deployCode("src/RootmodePot.vy", abi.encode(address(usdc), vault, GRACE)));
        client = vm.addr(clientKey);
        app = vm.addr(appKey);

        usdc.mint(client, 100e6);
        vm.prank(client);
        usdc.approve(address(pot), type(uint256).max);
        vm.prank(client);
        pot.deposit(100e6, MAX_JOB, MAX_DAY, app);
    }

    function test_deposit_with_permit_is_one_transaction() public {
        uint256 key = 0xB0B2;
        address other = vm.addr(key);
        usdc.mint(other, 10e6);
        assertEq(usdc.allowance(other, address(pot)), 0);

        uint256 deadline = block.timestamp + 1 hours;
        bytes32 structHash = keccak256(
            abi.encode(
                keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)"),
                other,
                address(pot),
                uint256(10e6),
                usdc.nonces(other),
                deadline
            )
        );
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", usdc.DOMAIN_SEPARATOR(), structHash));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);

        vm.prank(other);
        pot.depositWithPermit(10e6, MAX_JOB, MAX_DAY, app, deadline, v, r, s);

        (uint256 bal, uint256 maxJob,,,,) = pot.accounts(other);
        assertEq(bal, 10e6);
        assertEq(maxJob, MAX_JOB);
        assertEq(usdc.balanceOf(other), 0);
        assertEq(usdc.allowance(other, address(pot)), 0, "permit allowance is consumed by the pull");
        assertEq(usdc.nonces(other), 1);
    }

    function test_a_forged_permit_cannot_deposit() public {
        uint256 key = 0xB0B2;
        address other = vm.addr(key);
        usdc.mint(other, 10e6);
        uint256 deadline = block.timestamp + 1 hours;
        bytes32 structHash = keccak256(
            abi.encode(
                keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)"),
                other,
                address(pot),
                uint256(10e6),
                usdc.nonces(other),
                deadline
            )
        );
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", usdc.DOMAIN_SEPARATOR(), structHash));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(0xDEAD, digest);

        vm.prank(other);
        vm.expectRevert();
        pot.depositWithPermit(10e6, MAX_JOB, MAX_DAY, app, deadline, v, r, s);
    }

    function test_reserve_locks_funds_withdraw_cannot_take() public {
        uint64 deadline = uint64(block.timestamp + 1 hours);
        pot.reserve(client, worker, MAX_JOB, deadline, _reserve(worker, MAX_JOB, deadline));

        (uint256 free,,,,,) = pot.accounts(client);
        assertEq(free, 100e6 - MAX_JOB);
        assertEq(pot.locked(client, worker), MAX_JOB);

        vm.prank(client);
        pot.withdrawAll();
        assertEq(usdc.balanceOf(client), 100e6 - MAX_JOB, "only the free pot came back");
        assertEq(usdc.balanceOf(address(pot)), MAX_JOB, "the lock is still here");
        assertEq(pot.locked(client, worker), MAX_JOB);
    }

    function test_the_latest_ticket_pays_from_the_lock() public {
        uint64 deadline = uint64(block.timestamp + 1 hours);
        pot.reserve(client, worker, MAX_JOB, deadline, _reserve(worker, MAX_JOB, deadline));

        uint256 cumulative = 250_000;
        pot.settle(client, worker, cumulative, deadline, _spend(worker, cumulative, deadline));

        assertEq(usdc.balanceOf(worker), 225_000, "90% of $0.25");
        assertEq(usdc.balanceOf(vault), 25_000, "10%");
        assertEq(pot.locked(client, worker), MAX_JOB - cumulative);
        (uint256 free,,,,,) = pot.accounts(client);
        assertEq(free, 100e6 - MAX_JOB, "settle does not touch the free pot");
    }

    function test_a_later_ticket_pays_only_the_difference() public {
        uint64 deadline = uint64(block.timestamp + 1 hours);
        pot.reserve(client, worker, MAX_JOB, deadline, _reserve(worker, MAX_JOB, deadline));
        pot.settle(client, worker, 150_000, deadline, _spend(worker, 150_000, deadline));
        pot.settle(client, worker, 250_000, deadline, _spend(worker, 250_000, deadline));

        assertEq(usdc.balanceOf(worker), 225_000);
        assertEq(usdc.balanceOf(vault), 25_000);
        (uint256 paid,,,,,) = _channel();
        assertEq(paid, 250_000);
    }

    function test_an_old_or_equal_ticket_pays_nothing() public {
        uint64 deadline = uint64(block.timestamp + 1 hours);
        pot.reserve(client, worker, MAX_JOB, deadline, _reserve(worker, MAX_JOB, deadline));
        bytes memory latest = _spend(worker, 250_000, deadline);
        pot.settle(client, worker, 250_000, deadline, latest);

        vm.expectRevert("NotMonotonic");
        pot.settle(client, worker, 250_000, deadline, latest);

        bytes memory stale = _spend(worker, 150_000, deadline);
        vm.expectRevert("NotMonotonic");
        pot.settle(client, worker, 150_000, deadline, stale);
    }

    function test_settle_without_a_reserve_is_refused() public {
        uint64 deadline = uint64(block.timestamp + 1 hours);
        bytes memory sig = _spend(worker, 150_000, deadline);
        vm.expectRevert("NoReserve");
        pot.settle(client, worker, 150_000, deadline, sig);
    }

    function test_more_than_the_lock_is_refused() public {
        uint64 deadline = uint64(block.timestamp + 1 hours);
        pot.reserve(client, worker, 100_000, deadline, _reserve(worker, 100_000, deadline));
        bytes memory sig = _spend(worker, 150_000, deadline);
        vm.expectRevert("OverCap");
        pot.settle(client, worker, 150_000, deadline, sig);
    }

    function test_an_invented_ticket_without_the_app_key_is_refused() public {
        uint64 deadline = uint64(block.timestamp + 1 hours);
        pot.reserve(client, worker, MAX_JOB, deadline, _reserve(worker, MAX_JOB, deadline));
        bytes memory forged = _signSpend(clientKey, worker, 150_000, deadline);
        vm.expectRevert("BadSignature");
        pot.settle(client, worker, 150_000, deadline, forged);
    }

    function test_withdraw_does_not_kill_an_already_locked_settle() public {
        uint64 deadline = uint64(block.timestamp + 1 hours);
        pot.reserve(client, worker, MAX_JOB, deadline, _reserve(worker, MAX_JOB, deadline));
        vm.prank(client);
        pot.withdrawAll();
        // App key is gone, but the channel still honours the lock.
        pot.settle(client, worker, 150_000, deadline, _spend(worker, 150_000, deadline));
        assertEq(usdc.balanceOf(worker), 135_000);
    }

    function test_close_after_grace_returns_the_unused_lock() public {
        uint64 deadline = uint64(block.timestamp + 1 hours);
        pot.reserve(client, worker, MAX_JOB, deadline, _reserve(worker, MAX_JOB, deadline));
        pot.settle(client, worker, 150_000, deadline, _spend(worker, 150_000, deadline));

        vm.prank(client);
        pot.requestClose(worker);
        vm.expectRevert("TooSoon");
        pot.close(client, worker);

        vm.warp(block.timestamp + GRACE);
        pot.close(client, worker);
        (uint256 free,,,,,) = pot.accounts(client);
        assertEq(free, 100e6 - 150_000, "unused lock is free again");
        assertEq(pot.locked(client, worker), 0);
    }

    function test_billed_work_stays_the_workers_if_they_have_not_collected() public {
        uint64 deadline = uint64(block.timestamp + 1 hours);
        pot.reserve(client, worker, MAX_JOB, deadline, _reserve(worker, MAX_JOB, deadline));
        uint256 earned = 150_000;
        pot.commit(client, worker, earned, deadline, _spend(worker, earned, deadline));

        vm.prank(client);
        pot.requestClose(worker);
        vm.warp(block.timestamp + GRACE);
        pot.close(client, worker);

        (uint256 free,,,,,) = pot.accounts(client);
        assertEq(free, 100e6 - earned, "only unused reserve came back");
        assertEq(usdc.balanceOf(worker), 0, "not collected yet");
        assertEq(pot.locked(client, worker), earned);

        pot.collect(client, worker);
        assertEq(usdc.balanceOf(worker), 135_000);
        assertEq(usdc.balanceOf(vault), 15_000);
        assertEq(pot.locked(client, worker), 0);
    }

    function test_settle_still_works_after_the_client_asks_to_close() public {
        uint64 deadline = uint64(block.timestamp + 1 hours);
        pot.reserve(client, worker, MAX_JOB, deadline, _reserve(worker, MAX_JOB, deadline));
        vm.prank(client);
        pot.requestClose(worker);
        vm.warp(block.timestamp + GRACE);

        pot.settle(client, worker, 150_000, deadline, _spend(worker, 150_000, deadline));
        assertEq(usdc.balanceOf(worker), 135_000);
    }

    function test_print_digests() public {
        vm.chainId(8453);
        IRootmodePot pinned = IRootmodePot(0x1234567890AbcdEF1234567890aBcdef12345678);
        deployCodeTo("src/RootmodePot.vy", abi.encode(address(1), address(2), uint64(900)), address(pinned));

        bytes32 spend = keccak256(
            abi.encodePacked(
                "\x19\x01",
                pinned.domainSeparator(),
                keccak256(
                    abi.encode(
                        keccak256(
                            "SpendTicket(address client,address workerPayout,uint256 cumulative,uint64 deadline)"
                        ),
                        address(0x00000000000000000000000000000000000000A1),
                        address(0x00000000000000000000000000000000000000B0),
                        uint256(2_730_000),
                        uint256(1_700_000_000)
                    )
                )
            )
        );
        emit log_named_bytes32("spend ticket digest", spend);
        assertEq(spend, hex"9a758a5dc36ac9923b268e0e82c001fd8258f307aaaf2293d97b72c3e6544960");
    }

    function _channel() internal view returns (uint256 paid, uint256 reserved, uint64, uint64, address, uint256 earned) {
        (uint256 r, uint256 p, uint64 d, uint64 c, address k, uint256 e,,) = pot.channels(client, worker);
        return (p, r, d, c, k, e);
    }

    /// A client must not be able to lower its live per-job cap after work is
    /// delivered to block settlement and reclaim the whole reserve. The channel
    /// keeps the cap it had at reserve, so settle still pays the worker.
    function test_lowering_maxPerJob_cannot_block_a_reserved_settle() public {
        uint64 deadline = uint64(block.timestamp + 1 hours);
        pot.reserve(client, worker, MAX_JOB, deadline, _reserve(worker, MAX_JOB, deadline));

        // The grief attempt: zero the account's live caps after reserving.
        vm.prank(client);
        pot.setLimits(0, 0, address(0));

        // Settlement still succeeds against the channel's frozen snapshot.
        pot.settle(client, worker, 250_000, deadline, _spend(worker, 250_000, deadline));
        assertEq(usdc.balanceOf(worker), 225_000, "worker is paid despite the cap change");
        assertEq(usdc.balanceOf(vault), 25_000);
    }

    function _reserve(address payout, uint256 maxAmount, uint64 deadline) internal view returns (bytes memory) {
        return _signReserve(appKey, payout, maxAmount, deadline);
    }

    function _spend(address payout, uint256 cumulative, uint64 deadline) internal view returns (bytes memory) {
        return _signSpend(appKey, payout, cumulative, deadline);
    }

    function _signReserve(uint256 key, address payout, uint256 maxAmount, uint64 deadline)
        internal
        view
        returns (bytes memory)
    {
        bytes32 structHash = keccak256(
            abi.encode(
                keccak256("ReserveTicket(address client,address workerPayout,uint256 maxAmount,uint64 deadline)"),
                client,
                payout,
                maxAmount,
                uint256(deadline)
            )
        );
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", pot.domainSeparator(), structHash));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        return abi.encodePacked(r, s, v);
    }

    function _signSpend(uint256 key, address payout, uint256 cumulative, uint64 deadline)
        internal
        view
        returns (bytes memory)
    {
        bytes32 structHash = keccak256(
            abi.encode(
                keccak256("SpendTicket(address client,address workerPayout,uint256 cumulative,uint64 deadline)"),
                client,
                payout,
                cumulative,
                uint256(deadline)
            )
        );
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", pot.domainSeparator(), structHash));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        return abi.encodePacked(r, s, v);
    }
}
