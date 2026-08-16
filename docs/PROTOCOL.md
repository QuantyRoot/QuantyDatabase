# QuantyDB Wire Protocol, version 1

Normative description of the protocol spoken between a client and a
quanty server. If the code and this document disagree, one of them has a
bug and the fix must touch both.

Byte order is little endian everywhere, matching docs/FORMAT.md. Text is
UTF-8. The protocol is independent of how the server is built: ADR-022
makes it threads, but nothing below depends on that.

## What this is not

The value encoding here is **not** the order-preserving key encoding in
`quanty-core::encoding`, even where the two agree today. That encoding
exists to make memcmp match logical order and escapes zero bytes to do it;
this one exists to round-trip a value across a socket. They are separate
formats with separate version histories, and a change to one must not be
allowed to silently become a change to the other. Sharing the tag constants
would create exactly that coupling, so the tags below are defined here.

## Frames

Every message after the handshake is one frame:

```
offset  size  field
0       1     message type
1       4     body length in bytes, little endian
5       n     body
```

A frame is at most `5 + MAX_BODY` bytes, where **`MAX_BODY` is 16 MiB**.
A declared length above that is a protocol error and the connection closes
without reading the body. This bound is the whole reason a decoder can
allocate from a length field at all: the number arrives from the network,
so it is either capped before it reaches an allocator or it is a way to ask
for arbitrary memory. Capped it is.

Reading a frame therefore never allocates more than 16 MiB, and a decoder
that is handed nonsense returns an error rather than panicking. Both
properties are what the fuzzer checks.

## Handshake

The handshake is fixed for all time and is the only part of this document
that may never change. Everything after it is versioned; the handshake is
how the version is agreed, so it cannot itself be subject to negotiation.

Client sends exactly nine bytes, unframed:

```
offset  size  field
0       6     magic, ASCII "QUANTY"
6       2     protocol version, little endian u16
8       1     reserved, must be zero
```

Server replies with exactly four bytes, unframed:

```
offset  size  field
0       1     0x01 accepted, 0x00 refused
1       2     the version the server will speak, little endian u16
3       1     refusal reason if byte 0 is 0x00, else zero
```

On acceptance both sides speak the version in bytes 1..3, which is never
higher than the version the client asked for. On refusal the server closes
the connection immediately after the four bytes. Refusal reasons:

```
0x01  version too old, server no longer speaks it
0x02  version too new, server does not speak it yet
0x03  bad magic
```

This is the criterion "old client vs new server errors cleanly" reduced to
its smallest form: an old client that cannot parse anything else can still
parse four bytes and print why it was turned away.

## Message types

```
client to server
0x10  Auth          opaque token
0x11  Query         one statement, QQL
0x12  QuerySql      one statement, SQL
0x13  Close         orderly shutdown, no body

server to client
0x20  Ready         auth accepted, or no auth required
0x21  Ok            statement produced no rows
0x22  Count         verb plus a u64
0x23  RowsBegin     column count and names
0x24  RowBatch      a chunk of rows
0x25  RowsEnd       no body
0x26  Lines         a list of strings
0x27  Error         code plus message
```

Type bytes outside these ranges are a protocol error. The gaps are
deliberate: 0x14..0x1F and 0x28..0x2F are reserved so a later version can
add messages without moving anything.

## One request at a time

A connection has exactly one request in flight. The client sends one
`Query` and reads until it sees a terminal message (`Ok`, `Count`,
`RowsEnd`, `Lines` or `Error`) before sending another. There are no request
ids and no pipelining.

The price is real: a client cannot overlap requests on one connection and
must open a second connection to get concurrency. The reason to pay it is
that request ids on a protocol that cannot interleave responses are
decoration, and adding them later is a version bump that costs one field,
while removing them is not. This is the kind of thing that gets added when
something waits on it, per ADR-016.

## Values

A value is a one byte tag and a payload:

```
0x01  Null    no payload
0x02  Bool    1 byte, 0 or 1; any other byte is an error
0x03  Int     8 bytes, i64 little endian
0x04  Float   8 bytes, f64 bits little endian
0x05  Text    4 byte length, then that many bytes of UTF-8
0x06  Bytes   4 byte length, then that many bytes
```

Lengths are bounded by the remaining body, which is itself bounded by
`MAX_BODY`, so no length here can outrun the frame it lives in. Invalid
UTF-8 in `Text` is an error, not a replacement character: almost-right text
is the failure that survives, which is the same reasoning the SQLite reader
applies to unpaired surrogates.

Float carries bits rather than a decimal rendering, so NaN and both
infinities survive the trip and no value changes meaning by being sent.

## Result sets

`Output::Rows` is one Rust value but can be any size, so it does not fit
the frame cap as a single message. Rows arrive as a sequence:

```
RowsBegin   4 byte column count, then that many Text-encoded names
RowBatch    4 byte row count, then rows, each a 4 byte value count
            followed by that many values
RowsEnd     empty
```

A `RowBatch` is sized by the sender to stay under `MAX_BODY` and carries at
least one row. Zero rows is a valid result: `RowsBegin` then `RowsEnd`. An
`Error` may replace any `RowBatch`, which is how a failure partway through
a large result is reported; the client must treat rows already received as
belonging to a statement that did not finish.

## Errors

```
offset  size  field
0       2     error code, little endian u16
2       4     message length
6       n     message, UTF-8
```

The code is the contract and the message is for humans. The Rust error
enums are internal and free to change shape; the codes are not.

```
0x0001  protocol error, frame or encoding was malformed
0x0002  unsupported protocol version
0x0003  not authenticated
0x0004  authentication failed
0x0005  parse error
0x0006  execution error
0x0007  write queue rejected the statement
0x0008  server shutting down
```

`0x0007` is reserved rather than specified. ADR-003 gives the database one
writer, so the server serializes writers, and whether a client that waits
too long is told to retry or simply waits is an open question tied to how
`SuspendedTx` behaves when a connection holds an open transaction. The code
exists so that answering it later does not need a version bump.

## Authentication

`Auth` carries an opaque token: a 4 byte length and that many bytes. The
server replies `Ready` or `Error` with `0x0004`. A server that requires
auth answers `0x0003` to any `Query` that arrives before a successful
`Auth`; a server that does not require it may send `Ready` unprompted after
the handshake.

**Where tokens are stored and how they are revoked is not decided here.**
That is an open question for phase 5 and the token stays opaque on purpose:
every answer to it that has been raised so far fits behind these bytes, so
the format does not need to know the answer to be written down.

## Version history

```
1  first version. Handshake, one request in flight, six value tags.
```
