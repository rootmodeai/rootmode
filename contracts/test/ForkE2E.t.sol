// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {IRootmodeChannels, IFeeVault, IMockUSDC} from "../src/interfaces.sol";
import {IERC20} from "../src/IERC20.sol";
import {MockRouter} from "./FeeVault.t.sol";

/// One session on a Base fork, with the real USDC token.
///
/// Deposit → reserve → redeem (90/10) → redeem the difference → refuse a
/// replay → close the remainder → buyback the fees. That is the whole money
/// path the README draws. The buyback uses a mock router because there is no
/// project token pool yet; everything else is live Base state.
///
///   forge test --match-contract ForkE2E -vv
///
/// Override the RPC with BASE_RPC_URL if the public one is rate-limited.
contract ForkE2E is Test {
    /// Native Circle USDC on Base.
    IERC20 internal constant USDC = IERC20(0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913);

    uint64 internal constant EPOCH = 7 days;

    IRootmodeChannels internal channels;
    IFeeVault internal vault;
    IMockUSDC internal project;
    MockRouter internal router;

    uint256 internal clientKey = 0xA11CE;
    address internal client;
    address internal workerPayout = address(0xB0B);
    address internal sink = address(0x000000000000000000000000000000000000dEaD);

    bytes32 internal channelId;

    function setUp() public {
        string memory url = vm.envOr("BASE_RPC_URL", string("https://mainnet.base.org"));
        vm.createSelectFork(url);
        require(block.chainid == 8453, "fork is not Base");

        client = vm.addr(clientKey);
        channelId = keccak256(abi.encodePacked(client, workerPayout, "session-1"));

        project = IMockUSDC(deployCode("src/MockUSDC.vy"));
        router = new MockRouter(project);
        vault = IFeeVault(
            deployCode(
                "src/FeeVault.vy",
                abi.encode(address(USDC), address(project), address(router), uint24(3000), sink, EPOCH)
            )
        );
        channels = IRootmodeChannels(deployCode("src/RootmodeChannels.vy", abi.encode(address(USDC), address(vault))));

        _fundUsdc(client, 100e6);
        vm.prank(client);
        require(USDC.approve(address(channels), type(uint256).max), "approve");
    }

    function test_a_session_from_deposit_to_buyback() public {
        // --- deposit -------------------------------------------------------
        vm.prank(client);
        channels.deposit(client, 100e6);
        assertEq(channels.balanceOf(client), 100e6, "the deposit is the client's to spend");
        assertEq(USDC.balanceOf(address(channels)), 100e6);

        // --- reserve -------------------------------------------------------
        // The worker submits this: it is the one that needs the money locked
        // before it starts. $20, one day.
        uint64 deadline = uint64(block.timestamp + 1 days);
        channels.reserve(
            channelId, client, workerPayout, 20e6, deadline, _reserveSig(20e6, deadline)
        );
        assertEq(channels.balanceOf(client), 80e6, "only the earmark left the free balance");

        // The first job of a session has nothing to authorise yet. Nothing
        // happens on chain. The spend arrives with the *next* request.

        // --- redeem job 1, billed on request 2 -----------------------------
        // $2.73 of work delivered. 90% worker, 10% vault.
        bytes32 job1 = keccak256("job-1");
        channels.redeem(channelId, 2_730_000, job1, _spendSig(2_730_000, job1));
        assertEq(USDC.balanceOf(workerPayout), 2_457_000, "90% to the worker");
        assertEq(USDC.balanceOf(address(vault)), 273_000, "10% to the vault");

        // --- redeem the difference -----------------------------------------
        // Cumulative is now $4.00. Only the extra $1.27 moves.
        bytes32 job2 = keccak256("job-2");
        channels.redeem(channelId, 4e6, job2, _spendSig(4e6, job2));
        assertEq(USDC.balanceOf(workerPayout), 3_600_000);
        assertEq(USDC.balanceOf(address(vault)), 400_000);
        assertEq(USDC.balanceOf(workerPayout) + USDC.balanceOf(address(vault)), 4e6, "nothing stuck");

        // --- an old authorisation is worthless -----------------------------
        bytes memory stale = _spendSig(2_730_000, job1);
        vm.expectRevert("NotMonotonic");
        channels.redeem(channelId, 2_730_000, job1, stale);

        // --- more than was reserved cannot be claimed ----------------------
        bytes memory over = _spendSig(21e6, keccak256("greed"));
        vm.expectRevert("OverReserved");
        channels.redeem(channelId, 21e6, keccak256("greed"), over);

        // --- after the deadline the rest is the client's again -------------
        vm.expectRevert("StillOpen");
        channels.close(channelId);

        vm.warp(deadline + 1);
        channels.close(channelId);
        // $100 deposited, $4 earned, $16 of the reservation returned.
        assertEq(channels.balanceOf(client), 96e6);

        vm.prank(client);
        channels.withdraw(96e6);
        assertEq(USDC.balanceOf(client), 96e6);
        assertEq(USDC.balanceOf(address(channels)), 0, "the contract holds nothing of theirs");

        // --- buyback, once the epoch has elapsed ---------------------------
        // No project-token pool exists yet, so the router is a mock. The USDC
        // it spends is the real token the vault was paid in.
        vm.warp(block.timestamp + EPOCH);
        uint256 fees = 400_000;
        uint256 reward = (fees * 25) / 10_000;
        uint256 spent = fees - reward;
        uint256 callerBefore = USDC.balanceOf(address(this));

        uint256 received = vault.buyback(0);
        assertEq(received, spent * 2, "mock router pays 2:1");
        assertEq(project.balanceOf(sink), spent * 2, "bought tokens went to the sink");
        assertEq(USDC.balanceOf(address(this)) - callerBefore, reward, "the caller is paid for the gas");
        assertEq(vault.pending(), 0, "the epoch is empty");

        vm.expectRevert("TooSoon");
        vault.buyback(0);
    }

    // ------------------------------------------------------------- signing

    function _sign(bytes32 structHash) internal view returns (bytes memory) {
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", channels.domainSeparator(), structHash));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(clientKey, digest);
        return abi.encodePacked(r, s, v);
    }

    function _reserveSig(uint256 maxAmount, uint64 deadline) internal view returns (bytes memory) {
        return _sign(
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

    function _spendSig(uint256 cumulative, bytes32 metadataHash) internal view returns (bytes memory) {
        return _sign(
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

    /// Put real Base USDC in `to`. `deal` first; if this USDC's storage layout
    /// does not yield, mint as Circle's master minter on the fork.
    function _fundUsdc(address to, uint256 amount) internal {
        deal(address(USDC), to, amount, true);
        if (USDC.balanceOf(to) >= amount) return;

        (bool ok, bytes memory data) = address(USDC).call(abi.encodeWithSignature("masterMinter()"));
        require(ok && data.length >= 32, "USDC has no masterMinter");
        address master = abi.decode(data, (address));
        vm.prank(master);
        (ok,) = address(USDC).call(
            abi.encodeWithSignature("configureMinter(address,uint256)", address(this), type(uint256).max)
        );
        require(ok, "configureMinter");
        (ok,) = address(USDC).call(abi.encodeWithSignature("mint(address,uint256)", to, amount));
        require(ok, "mint");
        require(USDC.balanceOf(to) >= amount, "could not fund the client with Base USDC");
    }
}
