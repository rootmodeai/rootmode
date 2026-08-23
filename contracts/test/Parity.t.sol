// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {IRootmodeChannels} from "../src/interfaces.sol";

/// The one place the two implementations must agree exactly.
///
/// The worker checks an authorisation in Rust; the contract checks the same
/// bytes in Solidity. If the digests differ by a byte, every authorisation a
/// worker accepts is one it can never redeem — and it would find out only
/// after doing the work. So the digest is pinned to a fixture that
/// `payments::tests::the_digest_matches_the_contract` asserts against.
contract ParityTest is Test {
    function test_print_the_fixture_digest() public {
        // chainid and address are forced so the domain matches the Rust side.
        vm.chainId(8453);
        IRootmodeChannels c = IRootmodeChannels(0x1234567890AbcdEF1234567890aBcdef12345678);
        deployCodeTo("src/RootmodeChannels.vy", abi.encode(address(1), address(2)), address(c));

        bytes32 structHash = keccak256(
            abi.encode(
                keccak256("SpendingAuth(bytes32 channelId,address client,uint256 cumulative,bytes32 metadataHash)"),
                bytes32(uint256(0x11)),
                address(0x00000000000000000000000000000000000000A1),
                uint256(2_730_000),
                bytes32(uint256(0x22))
            )
        );
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", c.domainSeparator(), structHash));
        emit log_named_bytes32("spending digest", digest);
    }
}
