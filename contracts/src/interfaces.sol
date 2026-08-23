// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {IERC20} from "./IERC20.sol";

/// ABI of the Vyper contracts in this folder. The implementations are the `.vy` files.

interface IMockUSDC is IERC20 {
    function mint(address to, uint256 amount) external;
    function name() external view returns (string memory);
    function symbol() external view returns (string memory);
    function decimals() external view returns (uint8);
    function version() external view returns (string memory);
    function nonces(address owner) external view returns (uint256);
    function DOMAIN_SEPARATOR() external view returns (bytes32);
    function permit(
        address owner,
        address spender,
        uint256 value,
        uint256 deadline,
        uint8 v,
        bytes32 r,
        bytes32 s
    ) external;
}

interface ISwapRouter {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    function exactInputSingle(ExactInputSingleParams calldata params) external returns (uint256 amountOut);
}

interface IFeeVault {
    function usdc() external view returns (address);
    function admin() external view returns (address);
    function projectToken() external view returns (address);
    function router() external view returns (address);
    function poolFee() external view returns (uint24);
    function sink() external view returns (address);
    function epoch() external view returns (uint64);
    function lastBuyback() external view returns (uint64);
    function buyToken() external view returns (bool);
    function pending() external view returns (uint256);
    function nextBuyback() external view returns (uint64);
    function setAdmin(address newAdmin) external;
    function setSwap(address projectToken, address router, uint24 poolFee, address sink, uint64 epoch) external;
    function setBuyToken(bool enabled) external;
    function withdraw(address to, uint256 amount) external returns (uint256);
    function buyback(uint256 minOut) external returns (uint256);
}

interface IRootmodePot {
    function usdc() external view returns (address);
    function feeVault() external view returns (address);
    function grace() external view returns (uint64);
    function FEE_BPS() external view returns (uint16);
    function domainSeparator() external view returns (bytes32);
    function locked(address client, address worker) external view returns (uint256);
    function accounts(address)
        external
        view
        returns (
            uint256 balance,
            uint256 maxPerJob,
            uint256 maxPerDay,
            uint256 spentToday,
            uint64 dayStart,
            address appKey
        );
    function channels(address, address)
        external
        view
        returns (
            uint256 reserved,
            uint256 paid,
            uint64 deadline,
            uint64 closeAt,
            address appKey,
            uint256 earned
        );
    function deposit(uint256 amount, uint256 maxPerJob, uint256 maxPerDay, address appKey) external;
    function depositWithPermit(
        uint256 amount,
        uint256 maxPerJob,
        uint256 maxPerDay,
        address appKey,
        uint256 deadline,
        uint8 v,
        bytes32 r,
        bytes32 s
    ) external;
    function withdrawAll() external;
    function setLimits(uint256 maxPerJob, uint256 maxPerDay, address appKey) external;
    function reserve(address client, address workerPayout, uint256 maxAmount, uint64 deadline, bytes calldata appSig)
        external;
    function commit(address client, address workerPayout, uint256 cumulative, uint64 deadline, bytes calldata appSig)
        external;
    function settle(address client, address workerPayout, uint256 cumulative, uint64 deadline, bytes calldata appSig)
        external;
    function collect(address client, address workerPayout) external;
    function requestClose(address workerPayout) external;
    function close(address client, address workerPayout) external;
}

interface IRootmodeChannels {
    function token() external view returns (address);
    function feeVault() external view returns (address);
    function FEE_BPS() external view returns (uint16);
    function domainSeparator() external view returns (bytes32);
    function balanceOf(address) external view returns (uint256);
    function channels(bytes32)
        external
        view
        returns (address client, address workerPayout, uint256 reserved, uint256 paid, uint64 deadline);
    function deposit(address client, uint256 amount) external;
    function withdraw(uint256 amount) external;
    function reserve(
        bytes32 channelId,
        address client,
        address workerPayout,
        uint256 maxAmount,
        uint64 deadline,
        bytes calldata sig
    ) external;
    function redeem(bytes32 channelId, uint256 cumulative, bytes32 metadataHash, bytes calldata sig) external;
    function close(bytes32 channelId) external;
}
