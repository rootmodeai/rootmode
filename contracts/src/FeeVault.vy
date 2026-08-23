# pragma version 0.4.3
"""
The network's 10%.

Fees arrive here as USDC from the settlement contracts. Until the project
token exists, admin withdraws that USDC for operating costs. Flip `buyToken`
once the token is live; from then on admin spends each epoch's balance on
the token and sends it to `sink`.

Buyback is admin-only and time-boxed. Buying on every payment would be a
sandwich attached to every job. Batching to an epoch makes each buy large
relative to the fees an attacker can extract. minOut is the admin's price
bound — an oracle in here is one more thing to manipulate.
"""

from ethereum.ercs import IERC20

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
admin: public(address)
projectToken: public(address)
router: public(address)
poolFee: public(uint24)
# Where bought tokens go. Burning is 0xdead; a treasury is an address.
sink: public(address)
epoch: public(uint64)
lastBuyback: public(uint64)
# Off: withdraw USDC. On: buyback spends USDC on the project token.
buyToken: public(bool)

event BoughtBack:
    caller: indexed(address)
    spent: uint256
    received: uint256

event Withdrawn:
    to: indexed(address)
    amount: uint256

event AdminChanged:
    old: indexed(address)
    new: indexed(address)

event BuyTokenSet:
    enabled: bool

event SwapSet:
    projectToken: address
    router: address
    poolFee: uint24
    sink: address
    epoch: uint64


@deploy
def __init__(usdc_: address):
    """USDC only. Token, router, sink, epoch are set later with setSwap."""
    usdc = usdc_
    self.admin = msg.sender


@internal
def _only_admin():
    if msg.sender != self.admin:
        raise "OnlyAdmin"


@external
@view
def pending() -> uint256:
    """USDC collected and not yet spent or withdrawn."""
    return staticcall IERC20(usdc).balanceOf(self)


@external
@view
def nextBuyback() -> uint64:
    return self.lastBuyback + self.epoch


@external
def setAdmin(new_admin: address):
    self._only_admin()
    if new_admin == empty(address):
        raise "ZeroAdmin"
    old: address = self.admin
    self.admin = new_admin
    log AdminChanged(old=old, new=new_admin)


@external
def setSwap(
    projectToken_: address,
    router_: address,
    poolFee_: uint24,
    sink_: address,
    epoch_: uint64,
):
    """Point the vault at the pool once the token exists. Safe to call again."""
    self._only_admin()
    self.projectToken = projectToken_
    self.router = router_
    self.poolFee = poolFee_
    self.sink = sink_
    self.epoch = epoch_
    log SwapSet(
        projectToken=projectToken_,
        router=router_,
        poolFee=poolFee_,
        sink=sink_,
        epoch=epoch_,
    )


@external
def setBuyToken(enabled: bool):
    """
    Off: admin withdraws USDC. On: admin buybacks spend it on the token.
    Turning it on requires the swap path to be set.
    """
    self._only_admin()
    if enabled:
        if self.projectToken == empty(address) or self.router == empty(address) or self.sink == empty(address):
            raise "SwapUnset"
        if self.epoch == 0:
            raise "SwapUnset"
    self.buyToken = enabled
    log BuyTokenSet(enabled=enabled)


@external
def withdraw(to: address, amount: uint256) -> uint256:
    """
    Pull collected USDC. `amount == 0` sends the whole balance.
    Intended for operating costs before the token is live; still callable
    after, so a broken pool is not a locked box.
    """
    self._only_admin()
    if to == empty(address):
        raise "ZeroTo"
    bal: uint256 = staticcall IERC20(usdc).balanceOf(self)
    spent: uint256 = amount
    if amount == 0:
        spent = bal
    elif amount > bal:
        raise "Insufficient"
    if spent == 0:
        raise "NothingToWithdraw"
    ok: bool = extcall IERC20(usdc).transfer(to, spent)
    if not ok:
        raise "TransferFailed"
    log Withdrawn(to=to, amount=spent)
    return spent


@external
def buyback(minOut: uint256) -> uint256:
    """
    Spend this epoch's fees on the project token. No-op until `buyToken`.

    minOut is the admin's price bound — the least it will accept for the
    whole spend. Setting it to zero is accepting any fill.
    """
    self._only_admin()
    if not self.buyToken:
        raise "BuyTokenOff"
    if block.timestamp < convert(self.lastBuyback, uint256) + convert(self.epoch, uint256):
        raise "TooSoon"
    spend: uint256 = staticcall IERC20(usdc).balanceOf(self)
    if spend == 0:
        raise "NothingToSpend"

    # Written before the swap: the router is external code, and an epoch
    # that has begun should not be re-enterable.
    self.lastBuyback = convert(block.timestamp, uint64)

    approved: bool = extcall IERC20(usdc).approve(self.router, spend)
    if not approved:
        raise "ApproveFailed"
    received: uint256 = extcall ISwapRouter(self.router).exactInputSingle(
        ExactInputSingleParams(
            tokenIn=usdc,
            tokenOut=self.projectToken,
            fee=self.poolFee,
            recipient=self.sink,
            amountIn=spend,
            amountOutMinimum=minOut,
            sqrtPriceLimitX96=0,
        )
    )

    log BoughtBack(caller=msg.sender, spent=spend, received=received)
    return received
