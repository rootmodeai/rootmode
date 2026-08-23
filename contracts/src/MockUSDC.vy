# pragma version 0.4.3
"""Six-decimal token for local chains. Not the real thing."""

name: public(constant(String[12])) = "rootmode USD"
symbol: public(constant(String[4])) = "rUSD"
decimals: public(constant(uint8)) = 6

version: public(constant(String[1])) = "1"

balanceOf: public(HashMap[address, uint256])
allowance: public(HashMap[address, HashMap[address, uint256]])
nonces: public(HashMap[address, uint256])
totalSupply: public(uint256)

DOMAIN_TYPEHASH: constant(bytes32) = keccak256(
    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
)
PERMIT_TYPEHASH: constant(bytes32) = keccak256(
    "Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)"
)

event Transfer:
    sender: indexed(address)
    receiver: indexed(address)
    value: uint256

event Approval:
    owner: indexed(address)
    spender: indexed(address)
    value: uint256


@external
def mint(to: address, amount: uint256):
    self.balanceOf[to] += amount
    self.totalSupply += amount
    log Transfer(sender=empty(address), receiver=to, value=amount)


@external
def approve(spender: address, amount: uint256) -> bool:
    self.allowance[msg.sender][spender] = amount
    log Approval(owner=msg.sender, spender=spender, value=amount)
    return True


@external
def transfer(to: address, amount: uint256) -> bool:
    if self.balanceOf[msg.sender] < amount:
        raise "balance"
    self.balanceOf[msg.sender] -= amount
    self.balanceOf[to] += amount
    log Transfer(sender=msg.sender, receiver=to, value=amount)
    return True


@external
def transferFrom(sender: address, to: address, amount: uint256) -> bool:
    if self.allowance[sender][msg.sender] < amount:
        raise "allowance"
    if self.balanceOf[sender] < amount:
        raise "balance"
    self.allowance[sender][msg.sender] -= amount
    self.balanceOf[sender] -= amount
    self.balanceOf[to] += amount
    log Transfer(sender=sender, receiver=to, value=amount)
    return True


@external
@view
def DOMAIN_SEPARATOR() -> bytes32:
    return keccak256(
        abi_encode(
            DOMAIN_TYPEHASH,
            keccak256("rootmode USD"),
            keccak256("1"),
            chain.id,
            self,
        )
    )


@external
def permit(
    owner: address,
    spender: address,
    amount: uint256,
    deadline: uint256,
    v: uint8,
    r: bytes32,
    s: bytes32,
):
    if deadline < block.timestamp:
        raise "Expired"
    nonce: uint256 = self.nonces[owner]
    digest: bytes32 = keccak256(
        concat(
            b"\x19\x01",
            keccak256(
                abi_encode(
                    DOMAIN_TYPEHASH,
                    keccak256("rootmode USD"),
                    keccak256("1"),
                    chain.id,
                    self,
                )
            ),
            keccak256(abi_encode(PERMIT_TYPEHASH, owner, spender, amount, nonce, deadline)),
        )
    )
    signer: address = ecrecover(digest, convert(v, uint256), convert(r, uint256), convert(s, uint256))
    if signer == empty(address) or signer != owner:
        raise "BadSignature"
    self.nonces[owner] = nonce + 1
    self.allowance[owner][spender] = amount
    log Approval(owner=owner, spender=spender, value=amount)
