// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Script, console} from "forge-std/Script.sol";

/// FeeVault + RootmodePot on Base. USDC is Circle's.
///
///   forge script script/DeployBase.s.sol:DeployBase --broadcast \
///     --rpc-url "$BASE_RPC_URL" --private-key "$PRIVATE_KEY"
contract DeployBase is Script {
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;

    function run() external {
        require(block.chainid == 8453, "not Base");
        vm.startBroadcast();
        address vault = vm.deployCode("src/FeeVault.vy", abi.encode(USDC));
        address pot = vm.deployCode("src/RootmodePot.vy", abi.encode(USDC, vault, uint64(15 minutes)));
        vm.stopBroadcast();
        console.log("USDC", USDC);
        console.log("FEE_VAULT", vault);
        console.log("POT", pot);
        console.log("CHAIN_ID", block.chainid);
        console.log("ADMIN", msg.sender);
    }
}
