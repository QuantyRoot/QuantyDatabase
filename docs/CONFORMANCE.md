# Conformance checklist for a client

docs/PROTOCOL.md says what the wire format is. This page is how you find
out whether your client speaks it.

It is written for somebody implementing the protocol in another language,
so every check is stated in bytes and in nothing else. You do not need
Rust, you do not need cargo, and you should not need to open the server.
If a check cannot be followed without doing one of those, that is a bug
on this page and worth an issue.

Work through it in order against the fixture below. Each check says what
to send and what has to come back. The twelve cover every message type,
every value type, both front ends and every error code a client can
provoke on purpose. Three more at the end cover the refusals a client can
meet before it has sent a single frame.

The same checks run in Rust on every push, in
`crates/quantydb-cli/tests/wire.rs`. That file encodes this format by
hand from PROTOCOL.md, with no access to the encoder it is testing,
exactly as a new client has to. It is worth reading beside this page.

## The fixture

Every expected value below comes out of one database with one table and
two rows in it. Build it exactly as written, because the checks quote the
values back:

```
quantydb create conformance.qdb
quantydb run conformance.qdb 'table every { id: int @key, t: text, f: float, b: bool, raw: bytes, maybe: text @null }'
quantydb run conformance.qdb 'put every { id: 1, t: "hello", f: 1.5, b: true, raw: x"c0ffee" }'
quantydb run conformance.qdb 'put every { id: 2, t: "zwei", f: -0.25, b: false, raw: x"00", maybe: "here" }'
```

Three statements have run, so the branch sits at commit 3. Check 6 quotes
that number. Run anything else against the file first and it will differ.

Then mint a token:

```
quantydb token conformance
```

That prints two lines, a `token` and a `line`. They are halves of one
pair. The `token` half is the secret your client sends and is stored
nowhere, so copy it now; losing it means minting another. The `line` half
is what the server needs, so put it in a file of its own and serve:

```
echo '<the hash from the line half> conformance' > conformance.tokens
chmod 600 conformance.tokens
quantydb serve conformance.qdb --listen 127.0.0.1:7878 --tokens conformance.tokens
```

Mint once and copy by hand rather than piping `quantydb token` into the
file: the command mints a fresh pair every time it runs, and a pipeline
that keeps the `line` half throws away the `token` half you were about to
need.

The server says which address it took and warns that the wire is not
encrypted. Both are expected.

## The checks

### 1. The handshake is nine bytes out and four back

Send nine bytes, unframed:

```
51 55 41 4e 54 59 01 00 00
```

That is `QUANTY`, then 1 as a little endian u16, then the reserved zero.

Expect exactly four bytes back, unframed:

```
01 01 00 00
```

Byte 0 is 0x01 for accepted. Bytes 1 and 2 are the version both sides
will now speak. Byte 3 is zero.

Nothing here is framed. A client that reaches for a frame header at this
point eats the first byte of the message in check 2.

### 2. A `Ready` arrives without being asked for

Read one frame straight after the handshake. Type 0x20, body length zero,
so five bytes: `20 00 00 00 00`.

Every accepted handshake is followed by it, whether or not the server
requires a token. A client that does not read it finds it waiting as the
answer to whatever it sends next, which looks like the server replying to
the wrong message.

### 3. `Auth` with the token as printed is accepted

Send type 0x10. The body is a u32 length and that many bytes: the token
as the sixty-four characters `quantydb token` printed. So the body is 68
bytes and the whole frame is 73:

```
10 44 00 00 00 40 00 00 00 <64 characters>
```

The length inside the body is not optional and is not the frame length.
Every variable length field in this protocol carries its own, the token
included, and this is the first one a client meets. A bare token with no
length in front of it is answered with 0x0001, a protocol error, which
does not sound like the mistake it is.

Expect a frame of type 0x20 with an empty body.

### 4. The token decoded from hex is refused

Repeat check 3, but send the thirty-two bytes that those sixty-four
characters spell.

Expect type 0x27 with error code 0x0004. The server hashes whatever
arrives, so this is simply a different token, and the reply says nothing
about why. This is the mistake worth building a check around: the token
looks like hex, so decoding it is the natural thing to do, and it fails
in a way that reads like a wrong password.

### 5. Every value type survives the trip

Send type 0x11 with the body `get every`, so a u32 9 and nine
characters:

```
11 0d 00 00 00 09 00 00 00 67 65 74 20 65 76 65 72 79
```

Expect three frames.

`RowsBegin`, type 0x23: a count of 6, then six names as text.

```
id  t  f  b  raw  maybe
```

They are bare rather than `every.id` and so on, because the statement
reads one table.

`RowBatch`, type 0x24: a count of 2, then two rows, each a count of 6
followed by six values.

```
row 1   Int 1        Text "hello"   Float 1.5
        Bool true    Bytes c0ffee   Null
row 2   Int 2        Text "zwei"    Float -0.25
        Bool false   Bytes 00       Text "here"
```

`RowsEnd`, type 0x25, empty.

The floats are the point of this check. 1.5 and -0.25 are both exact in
binary, so a client that gets them back unchanged copied the bits rather
than rendering and reparsing a decimal. Null is a tag with no payload and
has to be told apart from an empty text, which is a tag, a length of
zero, and nothing else.

### 6. `Lines` comes back from `show branches`

Send type 0x11 with `show branches`.

Expect type 0x26: a count of 1, then one text.

```
* main @3
```

The number is the commit the branch sits at, which is 3 because the
fixture ran three statements. The star marks the branch you are on.

### 7. `Count` carries a verb and then a number

Send type 0x11 with:

```
put every { id: 3, t: "drei", f: 0.0, b: true, raw: x"ff" }
```

Expect type 0x22: the text `put`, then 1 as a little endian u64. Text
first and the number after it. A client that reads them the other way
round finds four plausible bytes where the length belongs and goes wrong
quietly, which is why this is its own check.

This statement adds a commit. Any later run of check 6 against the same
file sees `* main @4`.

### 8. The SQL front end answers on its own type

Send type 0x12 with:

```
SELECT id FROM every WHERE id = 2
```

Expect `RowsBegin` with one name, `RowBatch` with one row holding `Int
2`, then `RowsEnd`. Nothing but the type byte differs from check 5: both
front ends produce the same messages, and a client needs no second
decoder for SQL.

### 9. A statement before `Auth` is refused with 0x0003

Open a new connection, shake hands, read the `Ready`, then send a
`Query` without sending `Auth` first.

Expect type 0x27 with code 0x0003. Authentication is per connection, so
a token accepted on another connection does nothing here.

### 10. A wrong token is refused with 0x0004

On a new connection send `Auth` with sixty-four characters that are not
the token.

Expect type 0x27 with code 0x0004.

### 11. A parse error is 0x0005 and the connection stays open

Send type 0x11 with a body that is not a statement, for example:

```
this is not a statement
```

Expect type 0x27 with code 0x0005. Then send `get every` on the same
connection and expect it to be answered normally.

The second half is the check. A refused statement is not a refused
connection, and a client that tears down its socket on any error will
reconnect for every typo.

### 12. `Close` ends the stream

Send type 0x13 with an empty body: `13 00 00 00 00`.

Expect no further bytes and then end of stream. The server closes;
nothing is sent in reply.

### 13. A version the server does not speak is turned away

Hand the handshake a version of 2:

```
51 55 41 4e 54 59 02 00 00
```

Expect four bytes:

```
00 00 00 02
```

Byte 0 is 0x00 for refused and byte 3 is the reason, 0x02 for a version
too new. **Bytes 1 and 2 are zero in every refusal.** They carry the
version both sides will speak, and a refusing server will speak none, so
there is nothing to put there. Do not read them as the version the server
would have preferred.

The connection closes right after these four bytes.

### 14. A version the server no longer speaks is turned away

The same with a version of 0, which no server ever spoke:

```
00 00 00 01
```

Reason 0x01, a version too old.

### 15. Bad magic is turned away

Send nine bytes that do not begin with `QUANTY`.

```
00 00 00 03
```

Reason 0x03. This is the one refusal a client can hit by connecting to
something that is not a quantydb server at all, so it is worth telling
apart from the other two.

## When a check does not pass

Three things can be wrong, and it is worth working out which before
changing anything:

- **the client**, which is the usual answer, and the checks above are
  ordered so that the earliest failure is the most likely cause.
- **this page or docs/PROTOCOL.md**, if the bytes a check asks for are
  not the bytes the server sends. The document is normative, so a
  disagreement between it and the server is a bug in one of them and the
  fix touches both.
- **the server**, which `crates/quantydb-cli/tests/wire.rs` would
  normally have caught, but it only covers what somebody thought to
  write down.

The third is rarer than the first and likelier than it looks: every
mistake this page exists to prevent was found by somebody writing a
client against the document and getting a surprise. If a check surprises
you, that is worth an issue whichever of the three it turns out to be.
