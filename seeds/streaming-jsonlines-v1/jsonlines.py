class JSONLinesParser:
    def __init__(self, max_record_bytes):
        raise NotImplementedError

    def feed(self, chunk):
        raise NotImplementedError

    def finish(self):
        raise NotImplementedError
