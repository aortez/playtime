import json


class JSONLinesParser:
    def __init__(self, max_record_bytes):
        if type(max_record_bytes) is not int or max_record_bytes <= 0:
            raise ValueError('positive integer limit required')
        self.limit = max_record_bytes
        self.pending = bytearray()
        self.closed = False

    def _record(self):
        raw = bytes(self.pending)
        self.pending.clear()
        if not raw.strip(b' \t\r'):
            return []
        def bad_constant(value):
            raise ValueError('nonstandard JSON constant')
        return [json.loads(raw.decode('utf-8'), parse_constant=bad_constant)]

    def feed(self, chunk):
        if self.closed:
            raise RuntimeError('closed parser')
        try:
            out = []
            pos = 0
            while pos < len(chunk):
                end = chunk.find(b'\n', pos)
                stop = len(chunk) if end < 0 else end
                if len(self.pending) + stop - pos > self.limit:
                    raise ValueError('record too long')
                self.pending.extend(memoryview(chunk)[pos:stop])
                if end < 0:
                    break
                out.extend(self._record())
                pos = end + 1
            return out
        except (ValueError, UnicodeError):
            self.closed = True
            self.pending.clear()
            raise ValueError('invalid record') from None

    def finish(self):
        if self.closed:
            raise RuntimeError('closed parser')
        self.closed = True
        try:
            return self._record()
        except (ValueError, UnicodeError):
            raise ValueError('invalid record') from None
