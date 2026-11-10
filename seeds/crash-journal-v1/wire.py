"""Example frame encoder supplied as part of the task."""
import json
import zlib

MAGIC = b'PTJ1'
MAX_PAYLOAD = 16384


def frame(record):
    payload = json.dumps(record, ensure_ascii=True, separators=(',', ':')).encode('utf-8')
    return MAGIC + len(payload).to_bytes(4, 'big') + payload + (zlib.crc32(payload) & 0xffffffff).to_bytes(4, 'big')
