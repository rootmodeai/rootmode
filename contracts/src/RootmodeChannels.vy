# pragma version 0.4.3
"""
Where the money for inference sits and how it is paid out.

A client deposits USDC once and then signs, off-chain, what it has agreed to
pay each worker. Workers redeem those signatures whenever they like. Nothing
about a job — no prompt, no answer, not even the model name — touches this
contract; a job is a hash inside a signature the client made.

Three moves, and that is the whole protocol:

  deposit  → the client funds a balance
  reserve  → the client earmarks part of it for one worker, with a deadline
  redeem   → the worker presents the newest authorisation and is paid the
             difference against what it has already been paid

Redeeming is the only place value moves to a worker, and it moves only on the
client's own signature. The network's cut is taken here, once, at FEE_BPS, and
sent to the fee vault that buys the token back.
"""

from ethereum.ercs import IERC20

FEE_BPS: public(constant(uint16)) = 1000

DOMAIN_TYPEHASH: constant(bytes32) = keccak256(
    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
)
RESERVE_TYPEHASH: constant(bytes32) = keccak256(
    "ReserveAuth(bytes32 channelId,address client,address workerPayout,uint256 maxAmount,uint256 deadline)"
)
SPEND_TYPEHASH: constant(bytes32) = keccak256(
    "SpendingAuth(bytes32 channelId,address client,uint256 cumulative,bytes32 metadataHash)"
)
SECP256K1N_HALF: constant(uint256) = 57896044618658097711785492504343953926418782139537452191302581570759080747168

# USDC on Base. Six decimals, so every amount here is micros.
token: public(immutable(address))
feeVault: public(immutable(address))
_domainSeparator: immutable(bytes32)

# Unspent, unreserved balance per client.
balanceOf: public(HashMap[address, uint256])

struct Channel:
    client: address
    workerPayout: address
    reserved: uint256
    paid: uint256
    deadline: uint64

channels: public(HashMap[bytes32, Channel])

event Deposited:
    client: indexed(address)
    amount: uint256

event Withdrawn:
    client: indexed(address)
    amount: uint256

event Reserved:
    channelId: indexed(bytes32)
    client: indexed(address)
    worker: indexed(address)
    amount: uint256
    deadline: uint64

event Redeemed:
    channelId: indexed(bytes32)
    worker: indexed(address)
    paidToWorker: uint256
    fee: uint256
    metadataHash: bytes32

event Closed:
    channelId: indexed(bytes32)
    returned: uint256


@deploy
def __init__(usdc: address, vault: address):
    token = usdc
    feeVault = vault
    _domainSeparator = keccak256(
        abi_encode(
            DOMAIN_TYPEHASH,
            keccak256("RootmodeChannels"),
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
def deposit(client: address, amount: uint256):
    """Fund a balance. Anyone may fund anyone."""
    self._pull(amount)
    self.balanceOf[client] += amount
    log Deposited(client=client, amount=amount)


@external
def withdraw(amount: uint256):
    """Take back what is not reserved."""
    if self.balanceOf[msg.sender] < amount:
        raise "NotEnough"
    self.balanceOf[msg.sender] -= amount
    self._push(msg.sender, amount)
    log Withdrawn(client=msg.sender, amount=amount)


@external
def reserve(
    channelId: bytes32,
    client: address,
    workerPayout: address,
    maxAmount: uint256,
    deadline: uint64,
    sig: Bytes[65],
):
    """Earmark part of a balance for one worker until a deadline. Submitted by the worker, signed by the client."""
    if convert(deadline, uint256) <= block.timestamp:
        raise "Expired"
    digest: bytes32 = self._digest(
        keccak256(
            abi_encode(RESERVE_TYPEHASH, channelId, client, workerPayout, maxAmount, convert(deadline, uint256))
        )
    )
    if self._recover(digest, sig) != client:
        raise "BadSignature"

    if self.channels[channelId].client == empty(address):
        self.channels[channelId].client = client
        self.channels[channelId].workerPayout = workerPayout
        self.channels[channelId].deadline = deadline
    elif self.channels[channelId].client != client or self.channels[channelId].workerPayout != workerPayout:
        raise "WrongChannel"

    reserved: uint256 = self.channels[channelId].reserved
    if maxAmount <= reserved:
        raise "NotMonotonic"
    extra: uint256 = maxAmount - reserved
    if self.balanceOf[client] < extra:
        raise "NotEnough"
    self.balanceOf[client] -= extra
    self.channels[channelId].reserved = maxAmount
    if deadline > self.channels[channelId].deadline:
        self.channels[channelId].deadline = deadline
    log Reserved(channelId=channelId, client=client, worker=workerPayout, amount=maxAmount, deadline=self.channels[channelId].deadline)


@external
def redeem(channelId: bytes32, cumulative: uint256, metadataHash: bytes32, sig: Bytes[65]):
    """Be paid what the client has authorised, minus the network's share. cumulative is the total ever authorised."""
    if self.channels[channelId].client == empty(address):
        raise "WrongChannel"
    paid: uint256 = self.channels[channelId].paid
    if cumulative <= paid:
        raise "NotMonotonic"
    if cumulative > self.channels[channelId].reserved:
        raise "OverReserved"

    digest: bytes32 = self._digest(
        keccak256(abi_encode(SPEND_TYPEHASH, channelId, self.channels[channelId].client, cumulative, metadataHash))
    )
    if self._recover(digest, sig) != self.channels[channelId].client:
        raise "BadSignature"

    owed: uint256 = cumulative - paid
    self.channels[channelId].paid = cumulative

    # Rounded in the worker's favour: remainder stays on the larger side.
    fee: uint256 = (owed * convert(FEE_BPS, uint256)) // 10_000
    toWorker: uint256 = owed - fee
    worker: address = self.channels[channelId].workerPayout
    self._push(worker, toWorker)
    if fee > 0:
        self._push(feeVault, fee)
    log Redeemed(channelId=channelId, worker=worker, paidToWorker=toWorker, fee=fee, metadataHash=metadataHash)


@external
def close(channelId: bytes32):
    """After the deadline, return what was reserved and never earned."""
    if self.channels[channelId].client == empty(address):
        raise "WrongChannel"
    if block.timestamp <= convert(self.channels[channelId].deadline, uint256):
        raise "StillOpen"

    unspent: uint256 = self.channels[channelId].reserved - self.channels[channelId].paid
    self.channels[channelId].reserved = self.channels[channelId].paid
    self.balanceOf[self.channels[channelId].client] += unspent
    log Closed(channelId=channelId, returned=unspent)


@internal
def _pull(amount: uint256):
    ok: bool = extcall IERC20(token).transferFrom(msg.sender, self, amount)
    if not ok:
        raise "TransferFailed"


@internal
def _push(to: address, amount: uint256):
    ok: bool = extcall IERC20(token).transfer(to, amount)
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
