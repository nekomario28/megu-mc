#!/usr/bin/env python3
"""Region reads stay chunk-bounded; NBT floats retain IEEE-754 entity semantics."""
import gzip
import math
from pathlib import Path
import struct
import sys
import tempfile
import unittest
import zlib
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tools'))
import vanilla_region_blocks as region
from vanilla_region_blocks import parse_payload


class CountedFile:
    def __init__(self, file):
        self.file = file
        self.bytes_read = 0

    def __enter__(self):
        self.file.__enter__()
        return self

    def __exit__(self, *args):
        return self.file.__exit__(*args)

    def __getattr__(self, name):
        return getattr(self.file, name)

    def read(self, size=-1):
        data = self.file.read(size)
        self.bytes_read += len(data)
        return data


def write_region(world, cx, cz, body, *, kind=2, sector=3, sectors=1, length=None,
                 file_size=16 * 1024 * 1024):
    path = Path(world) / 'region' / f'r.{cx >> 5}.{cz >> 5}.mca'
    path.parent.mkdir(exist_ok=True)
    entry = sector.to_bytes(3, 'big') + bytes([sectors])
    with path.open('wb') as file:
        file.truncate(file_size)
        file.seek(((cx & 31) + (cz & 31) * 32) * 4)
        file.write(entry)
        file.seek(sector * 4096)
        file.write(struct.pack('>I', len(body) + 1 if length is None else length))
        file.write(bytes([kind]) + body)
    return path


class NbtRegionTests(unittest.TestCase):
    def test_chunk_read_does_not_scale_with_unrelated_region_bytes(self):
        payload = b'captured chunk payload' * 64
        body = zlib.compress(payload)
        with tempfile.TemporaryDirectory() as world:
            path = write_region(world, -1, -33, body)
            counted = CountedFile(path.open('rb'))
            with patch.object(region, 'open', return_value=counted, create=True):
                self.assertEqual(region.chunk_bytes(world, -1, -33), payload)
            # A 16MiB sparse region must not be loaded to read this one small chunk.
            self.assertLessEqual(counted.bytes_read, 4096 + 5 + len(body))

    def test_supported_compressions_return_exact_bytes_and_no_stale_cache(self):
        payload = b'\x0a\x00\x00\x03\x00\x01x\x00\x00\x00\x2a\x00'
        with tempfile.TemporaryDirectory() as world:
            for kind, body in [(1, gzip.compress(payload)), (2, zlib.compress(payload)), (3, payload)]:
                with self.subTest(kind=kind):
                    write_region(world, 32, -1, body, kind=kind)
                    self.assertEqual(region.chunk_bytes(world, 32, -1), payload)
            # Same path, new bytes: range reads must not retain an old world snapshot.
            changed = payload + b'new'
            write_region(world, 32, -1, changed, kind=3)
            self.assertEqual(region.chunk_bytes(world, 32, -1), changed)

    def test_multi_sector_payload_and_neighbor_slots(self):
        with tempfile.TemporaryDirectory() as world:
            # Uncompressed payload crosses a sector boundary and contains zero bytes.
            payload = bytes(range(256)) * 24
            path = write_region(world, 2, 3, payload, kind=3, sectors=2)
            with path.open('r+b') as file:
                file.seek((3 + 3 * 32) * 4)
                file.write((6).to_bytes(3, 'big') + b'\x01')
                file.seek(6 * 4096)
                file.write(struct.pack('>I', 4) + b'\x03xyz')
            self.assertEqual(region.chunk_bytes(world, 2, 3), payload)
            self.assertEqual(region.chunk_bytes(world, 3, 3), b'xyz')

    def test_missing_truncated_and_invalid_allocations_fail_closed(self):
        cases = [
            ({'sector': 0}, 'not present'),
            ({'sector': 1}, 'invalid sector'),
            ({'sectors': 0}, 'invalid sector'),
            ({'file_size': 3 * 4096 + 4, 'sector': 4}, 'invalid sector'),
            ({'length': 0}, 'invalid chunk length'),
            ({'length': 4093}, 'invalid chunk length'),
            ({'length': 2**31}, 'invalid chunk length'),
            ({'kind': 4}, 'unsupported compression 4'),
            ({'kind': 130}, 'unsupported compression 130'),
        ]
        with tempfile.TemporaryDirectory() as world:
            for kwargs, message in cases:
                with self.subTest(kwargs=kwargs):
                    path = write_region(world, 0, 0, b'x', **kwargs)
                    # The writer extends to the chunk it writes; truncate to model EOF.
                    if 'file_size' in kwargs:
                        with path.open('r+b') as file:
                            file.truncate(kwargs['file_size'])
                    with self.assertRaisesRegex(SystemExit, message):
                        region.chunk_bytes(world, 0, 0)
            path = write_region(world, -1, -1, b'abc', kind=3)
            with path.open('r+b') as file:
                file.truncate(2)
            with self.assertRaisesRegex(SystemExit, 'truncated location entry'):
                region.chunk_bytes(world, -1, -1)
            path = write_region(world, 0, 0, b'abc', kind=3, length=10)
            with path.open('r+b') as file:
                file.truncate(3 * 4096 + 8)
            with self.assertRaisesRegex(SystemExit, 'invalid chunk length'):
                region.chunk_bytes(world, 0, 0)

    def test_float_and_double_preserve_numeric_values_and_negative_zero(self):
        for kind, fmt in [(5, '>f'), (6, '>d')]:
            for value in [0.0, -0.0, 1.5, -2.25, float('inf'), float('-inf'), float('nan')]:
                with self.subTest(kind=kind, value=value):
                    payload = struct.pack(fmt, value)
                    decoded, end = parse_payload(payload, 0, kind)
                    self.assertEqual(end, len(payload))
                    if math.isnan(value):
                        self.assertTrue(math.isnan(decoded))
                    else:
                        self.assertEqual(decoded, value)
                        self.assertEqual(math.copysign(1.0, decoded), math.copysign(1.0, value))


if __name__ == '__main__':
    unittest.main()
