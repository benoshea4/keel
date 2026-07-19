#!/usr/bin/env python3
# S-FIX-3 fixture (status.md §S). A "forged-header" zip bomb: one deeply
# compressible entry whose UNCOMPRESSED-SIZE fields (local header + central
# directory) are hand-patched to 0. Python's zipfile writes honest headers, so
# the forge must be done by rewriting bytes — which is exactly the attacker
# capability the pre-fix pre-check (`entry.size()` == the forgeable header claim)
# trusted. The deflate stream still inflates to the real size, so a bounded read
# is the only defense; an unbounded read_to_end would materialise it in full.
#
#   forge_zipbomb.py <out.zip> <uncompressed_MiB>
#
# Emits a zip with a single entry "bomb.bin" = <MiB> MiB of zeros, deflated,
# with both uncompressed-size fields zeroed. Compressed size + CRC stay honest
# so any conformant reader still decompresses the whole stream.
import io, struct, sys, zipfile

out_path, mib = sys.argv[1], int(sys.argv[2])
payload = b"\x00" * (mib * 1024 * 1024)

buf = io.BytesIO()
with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
    # writestr on a seekable buffer back-patches sizes into the headers (no data
    # descriptor), so the fields we zero below are the ones the reader trusts.
    z.writestr("bomb.bin", payload)
data = bytearray(buf.getvalue())

# Zero the uncompressed-size u32 in the local file header (PK\x03\x04, +22) and
# in the central directory file header (PK\x01\x02, +24). Exactly one entry.
lfh = data.find(b"PK\x03\x04")
if lfh < 0:
    sys.exit("no local file header found")
struct.pack_into("<I", data, lfh + 22, 0)

cdh = data.find(b"PK\x01\x02")
if cdh < 0:
    sys.exit("no central directory header found")
struct.pack_into("<I", data, cdh + 24, 0)

with open(out_path, "wb") as f:
    f.write(data)
