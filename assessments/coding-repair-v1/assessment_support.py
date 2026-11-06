"""Trusted report transport for the container-only coding checkers."""

REPORTS = []


def emit_report(report):
    REPORTS.append(report)


class UnavailableModule:
    """Let each dependent case report an import failure without hiding others."""

    def __init__(self, error):
        self.error = f"{type(error).__name__}: {error}"[:1500]

    def __getattr__(self, name):
        raise RuntimeError("candidate could not be imported: " + self.error)
