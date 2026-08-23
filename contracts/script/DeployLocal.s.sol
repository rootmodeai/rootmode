// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Script, console} from "forge-std/Script.sol";
import {IMockUSDC, IFeeVault, IRootmodePot, ISwapRouter} from "../src/interfaces.sol";

contract LocalRouter is ISwapRouter {
    IMockUSDC public immutable tokenOut;
    constructor(IMockUSDC out) {
        tokenOut = out;
    }
    function exactInputSingle(ExactInputSingleParams calldata p) external override returns (uint256) {
        IMockUSDC(p.tokenIn).transferFrom(msg.sender, address(this), p.amountIn);
        uint256 out = p.amountIn * 2;
        require(out >= p.amountOutMinimum, "slippage");
        tokenOut.mint(p.recipient, out);
        return out;
    }
}

/// Local Anvil stack: fake USDC, a fee vault, the pot.
///
///   anvil
///   forge script script/DeployLocal.s.sol --broadcast --rpc-url http://127.0.0.1:8545 \
///     --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
contract DeployLocal is Script {
    function run() external {
        vm.startBroadcast();

        // The first few CREATE addresses from Anvil #0 (0x5FbD…, 0xe7f1725E…)
        // are famous and MetaMask caches them as other tokens with a 0
        // balance. Burn a handful of dummies so rUSD lands somewhere unique.
        new Dummy();
        new Dummy();
        new Dummy();
        new Dummy();
        new Dummy();

        IMockUSDC usdc = IMockUSDC(vm.deployCode("src/MockUSDC.vy"));
        IMockUSDC project = IMockUSDC(vm.deployCode("src/MockUSDC.vy"));
        LocalRouter router = new LocalRouter(project);
        IFeeVault vault = IFeeVault(
            vm.deployCode(
                "src/FeeVault.vy",
                abi.encode(
                    address(usdc),
                    address(project),
                    address(router),
                    uint24(3000),
                    address(0x000000000000000000000000000000000000dEaD),
                    uint64(7 days)
                )
            )
        );
        // 30s grace so a local withdraw-then-wait can reclaim unused lock.
        // Production would be 15 minutes.
        IRootmodePot pot = IRootmodePot(
            vm.deployCode("src/RootmodePot.vy", abi.encode(address(usdc), address(vault), uint64(30)))
        );

        // Anvil account 0 is the client in MetaMask. 10,000 USDC.
        usdc.mint(msg.sender, 10_000e6);

        vm.stopBroadcast();

        console.log("USDC", address(usdc));
        console.log("FEE_VAULT", address(vault));
        console.log("POT", address(pot));
        console.log("CLIENT", msg.sender);
        console.log("WORKER", 0x70997970C51812dc3A010C7d01b50e0d17dc79C8);
        console.log("CHAIN_ID", block.chainid);
    }
}

contract Dummy {}
