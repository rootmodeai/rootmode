# pragma version 0.4.3
"""
A client-owned pot with a locked per-worker reserve.

The wallet deposits, sets limits, and withdraws unlocked funds. That is the
whole of what a forked client can do: it cannot unlock a reserve by omitting
tickets from a transaction.

Before work, the app key signs a ReserveTicket and anyone (usually the worker)
posts it. That amount leaves the free balance and can only move to the worker
via a later SpendTicket, or back to the client after `grace` once they call
requestClose.

SpendTickets are cumulative. The newest supersedes the rest; settle pays the
difference from the lock, not from the free pot.
"""

from ethereum.ercs import IERC20

interface IERC20Permit:
    def permit(
        owner: address,
        spender: address,
        amount: uint256,
        deadline: uint256,
        v: uint8,
        r: bytes32,
        s: bytes32,
    ): nonpayable

FEE_BPS: public(constant(uint16)) = 1000
DAY: constant(uint256) = 86400

DOMAIN_TYPEHASH: constant(bytes32) = keccak256(
    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
)
RESERVE_TYPEHASH: constant(bytes32) = keccak256(
    "ReserveTicket(address client,address workerPayout,uint256 maxAmount,uint64 deadline)"
)
TICKET_TYPEHASH: constant(bytes32) = keccak256(
    "SpendTicket(address client,address workerPayout,uint256 cumulative,uint64 deadline)"
)
SECP256K1N_HALF: constant(uint256) = 57896044618658097711785492504343953926418782139537452191302581570759080747168

usdc: public(immutable(address))
feeVault: public(immutable(address))
# How long a worker has to settle after the client requests close.
grace: public(immutable(uint64))
_domainSeparator: immutable(bytes32)

struct Account:
    balance: uint256
    maxPerJob: uint256
    maxPerDay: uint256
    spentToday: uint256
    dayStart: uint64
    appKey: address

struct Channel:
    reserved: uint256
    paid: uint256
    deadline: uint64
    closeAt: uint64
    appKey: address
    # Highest spend ticket recorded. Close may only return reserved - earned.
    # Billed work stays the worker's even if they have not collected yet.
    earned: uint256
    # Per-job and per-day caps, snapshotted at the first reserve. Settlement is
    # checked against these, not the account's live limits, so lowering the
    # limits after work is delivered cannot block settle and reclaim the lock.
    # Appended last so the on-chain word layout the worker reads is unchanged.
    maxPerJob: uint256
    maxPerDay: uint256

accounts: public(HashMap[address, Account])
channels: public(HashMap[address, HashMap[address, Channel]])

event Deposited:
    client: indexed(address)
    amount: uint256
    maxPerJob: uint256
    maxPerDay: uint256
    appKey: address

event Withdrawn:
    client: indexed(address)
    amount: uint256

event LimitsSet:
    client: indexed(address)
    maxPerJob: uint256
    maxPerDay: uint256
    appKey: address

event Reserved:
    client: indexed(address)
    worker: indexed(address)
    reserved: uint256
    deadline: uint64

event Settled:
    client: indexed(address)
    worker: indexed(address)
    cumulative: uint256
    paidToWorker: uint256
    fee: uint256

event CloseRequested:
    client: indexed(address)
    worker: indexed(address)
    closeAt: uint64

event Closed:
    client: indexed(address)
    worker: indexed(address)
    returned: uint256


@deploy
def __init__(usdc_: address, vault: address, grace_: uint64):
    usdc = usdc_
    feeVault = vault
    grace = grace_
    _domainSeparator = keccak256(
        abi_encode(
            DOMAIN_TYPEHASH,
            keccak256("RootmodePot"),
            keccak256("1"),
            chain.id,
            self,
        )
    )


@external
@view
def domainSeparator() -> bytes32:
    return _domainSeparator


@external
@view
def locked(client: address, worker: address) -> uint256:
    """USDC still sitting here for this pair (unused reserve plus billed-but-uncollected)."""
    c: Channel = self.channels[client][worker]
    if c.reserved <= c.paid:
        return 0
    return c.reserved - c.paid


@external
def deposit(amount: uint256, maxPerJob: uint256, maxPerDay: uint256, appKey: address):
    if amount == 0:
        raise "ZeroAmount"
    self._pull(amount)
    self._credit(amount, maxPerJob, maxPerDay, appKey)


@external
def depositWithPermit(
    amount: uint256,
    maxPerJob: uint256,
    maxPerDay: uint256,
    appKey: address,
    deadline: uint256,
    v: uint8,
    r: bytes32,
    s: bytes32,
):
    """Approve (EIP-2612) and deposit in the same transaction."""
    if amount == 0:
        raise "ZeroAmount"
    extcall IERC20Permit(usdc).permit(msg.sender, self, amount, deadline, v, r, s)
    self._pull(amount)
    self._credit(amount, maxPerJob, maxPerDay, appKey)


@external
def withdrawAll():
    """Take the unlocked pot. Reserved funds stay until settle or close after grace."""
    amount: uint256 = self.accounts[msg.sender].balance
    self.accounts[msg.sender].balance = 0
    self.accounts[msg.sender].appKey = empty(address)
    if amount > 0:
        self._push(msg.sender, amount)
    log Withdrawn(client=msg.sender, amount=amount)


@external
def setLimits(maxPerJob: uint256, maxPerDay: uint256, appKey: address):
    self.accounts[msg.sender].maxPerJob = maxPerJob
    self.accounts[msg.sender].maxPerDay = maxPerDay
    if appKey != empty(address):
        self.accounts[msg.sender].appKey = appKey
    log LimitsSet(client=msg.sender, maxPerJob=maxPerJob, maxPerDay=maxPerDay, appKey=self.accounts[msg.sender].appKey)


@external
def reserve(
    client: address,
    workerPayout: address,
    maxAmount: uint256,
    deadline: uint64,
    appSig: Bytes[65],
):
    """Lock maxAmount of the client's free balance for workerPayout. Later tickets may only raise the ceiling."""
    if convert(deadline, uint256) <= block.timestamp:
        raise "Expired"
    if self.accounts[client].appKey == empty(address):
        raise "NoAppKey"

    digest: bytes32 = self._digest(
        keccak256(abi_encode(RESERVE_TYPEHASH, client, workerPayout, maxAmount, convert(deadline, uint256)))
    )
    if self._recover(digest, appSig) != self.accounts[client].appKey:
        raise "BadSignature"

    if self.channels[client][workerPayout].appKey == empty(address):
        self.channels[client][workerPayout].appKey = self.accounts[client].appKey
    elif self.channels[client][workerPayout].appKey != self.accounts[client].appKey:
        raise "BadSignature"

    # The caps a settle is checked against live on the channel, not on the
    # account, so a client cannot lower them after work is delivered to block
    # settlement and reclaim the lock. They still follow the client's wishes
    # in the two directions that are safe: they rise whenever the client
    # reserves with a higher account limit — raising can only help the worker
    # — and they start fresh from the account after a close, once the grace
    # period has given every worker its chance to settle. Between those, a
    # lowered account limit waits for the close.
    if self.channels[client][workerPayout].maxPerJob == 0 or self.accounts[client].maxPerJob > self.channels[client][workerPayout].maxPerJob:
        self.channels[client][workerPayout].maxPerJob = self.accounts[client].maxPerJob
    if self.channels[client][workerPayout].maxPerDay == 0 or self.accounts[client].maxPerDay > self.channels[client][workerPayout].maxPerDay:
        self.channels[client][workerPayout].maxPerDay = self.accounts[client].maxPerDay

    reserved: uint256 = self.channels[client][workerPayout].reserved
    if maxAmount <= reserved:
        raise "NotMonotonic"
    extra: uint256 = maxAmount - reserved
    if self.accounts[client].balance < extra:
        raise "NotEnough"
    self.accounts[client].balance -= extra
    self.channels[client][workerPayout].reserved = maxAmount
    if deadline > self.channels[client][workerPayout].deadline:
        self.channels[client][workerPayout].deadline = deadline
    self.channels[client][workerPayout].closeAt = 0
    log Reserved(client=client, worker=workerPayout, reserved=maxAmount, deadline=self.channels[client][workerPayout].deadline)


@external
def commit(
    client: address,
    workerPayout: address,
    cumulative: uint256,
    deadline: uint64,
    appSig: Bytes[65],
):
    """Record a spend ticket as earned without transferring. Close cannot take this back."""
    self._recognize(client, workerPayout, cumulative, deadline, appSig)


@external
def settle(
    client: address,
    workerPayout: address,
    cumulative: uint256,
    deadline: uint64,
    appSig: Bytes[65],
):
    """Pay earned - paid from the lock. Anyone can call; funds only ever go to workerPayout."""
    self._recognize(client, workerPayout, cumulative, deadline, appSig)
    self._pay(client, workerPayout)


@external
def collect(client: address, workerPayout: address):
    """Collect already-committed earnings."""
    self._pay(client, workerPayout)


@external
def requestClose(workerPayout: address):
    """Start the grace period. Unused lock returns after grace."""
    if self.channels[msg.sender][workerPayout].appKey == empty(address):
        raise "NoReserve"
    if self.channels[msg.sender][workerPayout].reserved <= self.channels[msg.sender][workerPayout].earned:
        raise "NoReserve"
    closeAt: uint64 = convert(block.timestamp, uint64) + grace
    self.channels[msg.sender][workerPayout].closeAt = closeAt
    log CloseRequested(client=msg.sender, worker=workerPayout, closeAt=closeAt)


@external
def close(client: address, workerPayout: address):
    """Return unused lock (reserved - earned). Billed work stays for the worker to collect."""
    if self.channels[client][workerPayout].appKey == empty(address):
        raise "NoReserve"
    closeAt: uint64 = self.channels[client][workerPayout].closeAt
    deadline: uint64 = self.channels[client][workerPayout].deadline
    timedOut: bool = closeAt != 0 and block.timestamp >= convert(closeAt, uint256)
    expired: bool = deadline != 0 and block.timestamp > convert(deadline, uint256)
    if not timedOut and not expired:
        raise "TooSoon"
    unspent: uint256 = self.channels[client][workerPayout].reserved - self.channels[client][workerPayout].earned
    self.channels[client][workerPayout].reserved = self.channels[client][workerPayout].earned
    self.channels[client][workerPayout].closeAt = 0
    # A closed channel takes the account's limits afresh on its next reserve,
    # lower ones included: the grace period (or the deadline) has passed, so
    # every worker has had its chance to settle what was billed under the old
    # caps. Anything still uncollected settles against `earned`, which the
    # caps do not gate.
    self.channels[client][workerPayout].maxPerJob = 0
    self.channels[client][workerPayout].maxPerDay = 0
    if unspent > 0:
        self.accounts[client].balance += unspent
    log Closed(client=client, worker=workerPayout, returned=unspent)


@internal
def _recognize(
    client: address,
    workerPayout: address,
    cumulative: uint256,
    deadline: uint64,
    appSig: Bytes[65],
):
    if self.channels[client][workerPayout].appKey == empty(address):
        raise "NoReserve"
    if block.timestamp > convert(deadline, uint256):
        raise "Expired"
    earned: uint256 = self.channels[client][workerPayout].earned
    if cumulative <= earned:
        raise "NotMonotonic"
    if cumulative > self.channels[client][workerPayout].reserved:
        raise "OverCap"
    delta: uint256 = cumulative - earned

    # Caps come from the channel's snapshot, taken at reserve, not the account's
    # live limits — a client cannot lower them after work to block settlement.
    if self.channels[client][workerPayout].maxPerJob == 0 or delta > self.channels[client][workerPayout].maxPerJob:
        raise "OverCap"
    self._rollDay(client)
    maxPerDay: uint256 = self.channels[client][workerPayout].maxPerDay
    if maxPerDay > 0 and self.accounts[client].spentToday + delta > maxPerDay:
        raise "OverCap"

    digest: bytes32 = self._digest(
        keccak256(abi_encode(TICKET_TYPEHASH, client, workerPayout, cumulative, convert(deadline, uint256)))
    )
    if self._recover(digest, appSig) != self.channels[client][workerPayout].appKey:
        raise "BadSignature"

    self.channels[client][workerPayout].earned = cumulative
    self.accounts[client].spentToday += delta


@internal
def _pay(client: address, workerPayout: address):
    earned: uint256 = self.channels[client][workerPayout].earned
    paid: uint256 = self.channels[client][workerPayout].paid
    if earned <= paid:
        raise "NotMonotonic"
    amount: uint256 = earned - paid
    self.channels[client][workerPayout].paid = earned

    fee: uint256 = (amount * convert(FEE_BPS, uint256)) // 10_000
    toWorker: uint256 = amount - fee
    self._push(workerPayout, toWorker)
    if fee > 0:
        self._push(feeVault, fee)
    log Settled(client=client, worker=workerPayout, cumulative=earned, paidToWorker=toWorker, fee=fee)


@internal
def _rollDay(client: address):
    dayStart: uint64 = self.accounts[client].dayStart
    if dayStart == 0 or block.timestamp >= convert(dayStart, uint256) + DAY:
        self.accounts[client].dayStart = convert(block.timestamp, uint64)
        self.accounts[client].spentToday = 0


@internal
def _credit(amount: uint256, maxPerJob: uint256, maxPerDay: uint256, appKey: address):
    self.accounts[msg.sender].balance += amount
    if maxPerJob > 0:
        self.accounts[msg.sender].maxPerJob = maxPerJob
    if maxPerDay > 0:
        self.accounts[msg.sender].maxPerDay = maxPerDay
    if appKey != empty(address):
        self.accounts[msg.sender].appKey = appKey
    a: Account = self.accounts[msg.sender]
    log Deposited(client=msg.sender, amount=amount, maxPerJob=a.maxPerJob, maxPerDay=a.maxPerDay, appKey=a.appKey)


@internal
def _pull(amount: uint256):
    ok: bool = extcall IERC20(usdc).transferFrom(msg.sender, self, amount)
    if not ok:
        raise "TransferFailed"


@internal
def _push(to: address, amount: uint256):
    ok: bool = extcall IERC20(usdc).transfer(to, amount)
    if not ok:
        raise "TransferFailed"


@internal
@view
def _digest(structHash: bytes32) -> bytes32:
    return keccak256(concat(b"\x19\x01", _domainSeparator, structHash))


@internal
@pure
def _recover(digest: bytes32, sig: Bytes[65]) -> address:
    if len(sig) != 65:
        raise "BadSignature"
    r: uint256 = convert(slice(sig, 0, 32), uint256)
    s: uint256 = convert(slice(sig, 32, 32), uint256)
    v: uint256 = convert(slice(sig, 64, 1), uint256)
    if v < 27:
        v += 27
    if s > SECP256K1N_HALF:
        raise "BadSignature"
    signer: address = ecrecover(digest, v, r, s)
    if signer == empty(address):
        raise "BadSignature"
    return signer
