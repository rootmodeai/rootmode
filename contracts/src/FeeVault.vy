# pragma version 0.4.3
"""
The network's 10%, turned into token buybacks once per epoch.

Fees arrive here as USDC from the settlement contracts. Once per epoch anyone
may call buyback, which spends the balance on the project token and sends it
to `sink`.

Two deliberate choices:

* Anyone can call it. A buyback that depends on the team remembering is a
  promise; one anybody can trigger, on a schedule the contract enforces, is a
  mechanism. The caller pays gas and keeps CALLER_REWARD_BPS of the epoch for
  doing so, which is what makes somebody bother.
* Time-boxed, not continuous. Buying on every payment would be a sandwich
  opportunity attached to every single job. Batching to an epoch makes each
  buy large relative to the fees an attacker can extract.

The price bound comes from the caller, not from an oracle in here: an oracle
is one more thing to be manipulated, and minOut lets whoever is paying the
gas refuse a bad fill.
"""

from ethereum.ercs import IERC20

CALLER_REWARD_BPS: public(constant(uint16)) = 25  # 0.25%

struct ExactInputSingleParams:
    tokenIn: address
    tokenOut: address
    fee: uint24
    recipient: address
    amountIn: uint256
    amountOutMinimum: uint256
    sqrtPriceLimitX96: uint160

interface ISwapRouter:
    def exactInputSingle(params: ExactInputSingleParams) -> uint256: nonpayable

usdc: public(immutable(address))
projectToken: public(immutable(address))
router: public(immutable(address))
poolFee: public(immutable(uint24))
# Where bought tokens go. Burning is 0xdead; a treasury is an address.
sink: public(immutable(address))
epoch: public(immutable(uint64))
lastBuyback: public(uint64)

event BoughtBack:
    caller: indexed(address)
    spent: uint256
    received: uint256
    callerReward: uint256


@deploy
def __init__(
    usdc_: address,
    projectToken_: address,
    router_: address,
    poolFee_: uint24,
    sink_: address,
    epoch_: uint64,
):
    usdc = usdc_
    projectToken = projectToken_
    router = router_
    poolFee = poolFee_
    sink = sink_
    epoch = epoch_
    self.lastBuyback = convert(block.timestamp, uint64)


@external
@view
def pending() -> uint256:
    """USDC collected and not yet spent."""
    return staticcall IERC20(usdc).balanceOf(self)


@external
@view
def nextBuyback() -> uint64:
    return self.lastBuyback + epoch


@external
def buyback(minOut: uint256) -> uint256:
    """
    Spend this epoch's fees on the project token.

    minOut is the caller's price bound — the least it will accept for the
    whole spend. Setting it to zero is accepting any fill.
    """
    if block.timestamp < convert(self.lastBuyback, uint256) + convert(epoch, uint256):
        raise "TooSoon"
    balance: uint256 = staticcall IERC20(usdc).balanceOf(self)
    if balance == 0:
        raise "NothingToSpend"

    # Written before the swap: the router is external code, and an epoch
    # that has begun should not be re-enterable.
    self.lastBuyback = convert(block.timestamp, uint64)

    reward: uint256 = (balance * convert(CALLER_REWARD_BPS, uint256)) // 10_000
    spend: uint256 = balance - reward

    extcall IERC20(usdc).approve(router, spend)
    received: uint256 = extcall ISwapRouter(router).exactInputSingle(
        ExactInputSingleParams(
            tokenIn=usdc,
            tokenOut=projectToken,
            fee=poolFee,
            recipient=sink,
            amountIn=spend,
            amountOutMinimum=minOut,
            sqrtPriceLimitX96=0,
        )
    )

    if reward > 0:
        extcall IERC20(usdc).transfer(msg.sender, reward)
    log BoughtBack(caller=msg.sender, spent=spend, received=received, callerReward=reward)
    return received
