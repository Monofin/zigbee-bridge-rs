"""Public scalar server attributes from ZCL R8 section 3.14, tables 3-78 through 3-86."""


def io_attributes():
    for cluster in range(0x000c, 0x0015):
        analog = cluster <= 0x000e
        binary = 0x000f <= cluster <= 0x0011
        value_type = 0x39 if analog else 0x10 if binary else 0x21
        rows = [
            (0x001c, 0x42, "description"), (0x0051, 0x10, "out of service"),
            (0x0055, value_type, "present value"), (0x0067, 0x30, "reliability"),
            (0x006f, 0x18, "status flags"), (0x0100, 0x23, "application type"),
            (0xfffd, 0x21, "cluster revision"), (0xfffe, 0x30, "reporting status"),
        ]
        if analog:
            rows.append((0x0075, 0x31, "engineering units"))
        if cluster in (0x000c, 0x000d):
            rows.extend([(0x0041, 0x39, "max present value"), (0x0045, 0x39, "min present value"),
                         (0x006a, 0x39, "resolution")])
        if binary:
            rows.extend([(0x0004, 0x42, "active text"), (0x002e, 0x42, "inactive text")])
        if cluster in (0x000f, 0x0010):
            rows.append((0x0054, 0x30, "polarity"))
        if cluster in (0x0010, 0x0011):
            rows.extend([(0x0042, 0x23, "minimum off time"), (0x0043, 0x23, "minimum on time")])
        if cluster >= 0x0012:
            rows.append((0x004a, 0x21, "number of states"))
        if cluster not in (0x000c, 0x000f, 0x0012):
            rows.append((0x0068, value_type, "relinquish default"))
        for attribute, wire_type, name in rows:
            yield cluster, attribute, wire_type, name
