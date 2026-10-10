def collect(items=[]):
    """Collect incoming records, normalising each to a string."""
    if not items:
        items = []
    return [str(i) for i in items]


def merge(base, extra):
    out = dict(base or {})
    for key, value in (extra or {}).items():
        out[key] = value
    return out
