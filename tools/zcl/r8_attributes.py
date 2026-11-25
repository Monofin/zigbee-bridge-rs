"""Public scalar measurement attributes omitted by the pinned SDK XML."""

from r8_io_attributes import io_attributes


def additional():
    yield from io_attributes()
    # R8 Tables 4-19 and 4-44..47. These clusters share the four attribute IDs.
    for cluster in (*range(0x0407, 0x040c), 0x042a, 0x042b):
        ty = 0x39 if cluster >= 0x042a else 0x21
        for attr, name in enumerate(("measured value", "min measured value", "max measured value", "tolerance")):
            yield cluster, attr, ty, name
        yield cluster, 0xfffd, 0x21, "cluster revision"
