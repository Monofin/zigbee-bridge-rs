use zigbee_zcl::{
    Direction, Frame, FrameType, Header, PayloadError,
    attributes::{AttributeValue, Operation, SelectedAttribute, Selector, StructuredWrite},
    discovery::{AttributeInfo, ExtendedAttributeInfo, Page},
    foundation::Payload,
    value::{DataType, Value},
};

fn frame(id: u8, bytes: &[u8]) -> Frame<'_> {
    Frame {
        header: Header {
            frame_type: FrameType::General,
            manufacturer: None,
            direction: Direction::ClientToServer,
            disable_default_response: false,
            sequence: 7,
            command_id: id,
        },
        payload: bytes,
    }
}

#[test]
fn every_command_has_a_wire_vector_with_shared_layouts_exercised_together() {
    // Each entry is one layout from the specification, paired with the command
    // IDs which use it. These vectors are independent of the codec schema.
    let cases: &[(&[u8], &[u8])] = &[
        (&[0x00], &[0x34, 0x12, 0xef, 0xbe]),
        (
            &[0x01],
            &[0x34, 0x12, 0, 0x21, 0xcd, 0xab, 0xef, 0xbe, 0x86],
        ),
        (&[0x02, 0x03, 0x05, 0x0a], &[0x34, 0x12, 0x21, 0xcd, 0xab]),
        (&[0x04, 0x07, 0x10], &[0]),
        (
            &[0x06],
            &[0, 0x34, 0x12, 0x21, 2, 0, 10, 0, 3, 0, 1, 0xef, 0xbe, 9, 0],
        ),
        (&[0x08], &[1, 0x34, 0x12]),
        (
            &[0x09],
            &[0x86, 1, 0xef, 0xbe, 0, 0, 0x34, 0x12, 0x10, 2, 0, 9, 0],
        ),
        (&[0x0b], &[0x15, 0x81]),
        (&[0x0c, 0x15], &[0x34, 0x12, 8]),
        (&[0x0d], &[1, 0x34, 0x12, 0x21]),
        (&[0x0e], &[0x34, 0x12, 2, 1, 0, 0x34, 0x12]),
        (&[0x0f], &[0x34, 0x12, 0x11, 1, 0, 0x20, 2]),
        (&[0x11, 0x13], &[0x34, 8]),
        (&[0x12, 0x14], &[1, 0x11, 0x22]),
        (&[0x16], &[0, 0x34, 0x12, 0x21, 7]),
    ];
    let mut covered = [false; 23];
    for &(ids, wire) in cases {
        for &id in ids {
            let value = Payload::decode(frame(id, wire)).unwrap();
            assert_eq!(value.command() as u8, id);
            assert_eq!(value.encode().unwrap(), wire);
            assert!(!std::mem::replace(&mut covered[usize::from(id)], true));
        }
    }
    assert!(covered.into_iter().all(|v| v));
}

#[test]
fn hand_authored_values_match_the_structured_write_and_discovery_formats() {
    let value = Value::from_bytes(DataType::try_from(0x21).unwrap(), &[0xcd, 0xab]).unwrap();
    let attribute = AttributeValue { id: 0x1234, value };
    let writes = Payload::WriteAttributesUndivided(vec![attribute]);
    assert_eq!(writes.encode().unwrap(), [0x34, 0x12, 0x21, 0xcd, 0xab]);
    let structured = Payload::WriteAttributesStructured(vec![StructuredWrite {
        attribute: SelectedAttribute {
            id: 0x1234,
            selector: Selector {
                operation: Operation::Remove,
                indexes: vec![0x4567],
            },
        },
        value,
    }]);
    let wire = [0x34, 0x12, 0x21, 0x67, 0x45, 0x21, 0xcd, 0xab];
    assert_eq!(structured.encode().unwrap(), wire);
    assert_eq!(Payload::decode(frame(0x0f, &wire)).unwrap(), structured);
    let page = Payload::DiscoverAttributesExtendedResponse(Page {
        complete: true,
        entries: vec![ExtendedAttributeInfo {
            attribute: AttributeInfo {
                id: 0x1234,
                data_type: 0x21,
            },
            access: 7,
        }],
    });
    assert_eq!(
        Payload::decode(frame(0x16, &[1, 0x34, 0x12, 0x21, 7])).unwrap(),
        page
    );
}

#[test]
fn conditional_layouts_handle_failures_and_empty_discovery_pages() {
    for (id, body) in [
        (0x04, &[0x86, 1, 0][..]),
        (0x07, &[0x86, 1, 1, 0]),
        (0x10, &[0x86, 1, 0, 1, 2, 0]),
        (0x09, &[0, 1, 1, 0, 10, 0]),
    ] {
        assert_eq!(
            Payload::decode(frame(id, body)).unwrap().encode().unwrap(),
            body
        );
    }
    for id in [0x0d, 0x12, 0x14, 0x16] {
        assert_eq!(
            Payload::decode(frame(id, &[1])).unwrap().encode().unwrap(),
            [1]
        );
        assert!(Payload::decode(frame(id, &[2])).is_err());
    }
    for (id, body) in [
        (0x0b, &[1, 2, 3][..]),
        (0x0c, &[1, 2]),
        (0x0d, &[0, 2, 3]),
        (0x16, &[1, 2, 3, 4]),
        (0x07, &[0, 0x86, 1, 1, 0]),
        (0x06, &[2, 1, 0]),
        (0x0e, &[1, 0, 0x10]),
        (0x0f, &[1, 0, 0x30, 0x21, 0, 0]),
        (0x06, &[0, 1, 0, 0x21, 0, 0, 9, 0, 3]),
    ] {
        assert!(Payload::decode(frame(id, body)).is_err(), "id={id:#04x}");
    }
    let mut local = frame(0x0c, &[0, 0, 1]);
    local.header.frame_type = FrameType::ClusterSpecific;
    assert_eq!(Payload::decode(local), Err(PayloadError::NotGeneral));
    assert_eq!(
        Payload::decode(frame(0xab, &[])),
        Err(PayloadError::UnassignedCommand(0xab))
    );
}

#[test]
fn value_lengths_cover_the_r8_type_table_and_nested_collections() {
    // Table 2-11 groups of fixed-width identifiers. Test every width and reject
    // trailing or missing bytes, including the zero-width null types.
    let fixed: &[(u8, &[u8])] = &[
        (0, &[0, 0xff]),
        (1, &[0x10, 0x30]),
        (2, &[0x31, 0x38, 0xe8, 0xe9]),
        (4, &[0x39, 0xe0, 0xe1, 0xe2, 0xea]),
        (8, &[0x3a, 0xf0]),
        (16, &[0xf1]),
    ];
    let mut entries = Vec::new();
    for &(width, ids) in fixed {
        entries.extend(ids.iter().map(|&id| (id, usize::from(width))));
    }
    for base in [0x08, 0x18, 0x20, 0x28] {
        entries.extend((0..8).map(|n| (base + n, usize::from(n) + 1)));
    }
    let data = [0x55; 17];
    for (id, width) in entries {
        let t = DataType::try_from(id).unwrap();
        assert_eq!(
            Value::from_bytes(t, &data[..width]).unwrap().as_bytes(),
            &data[..width]
        );
        assert!(Value::from_bytes(t, &data[..width + 1]).is_err());
        if width > 0 {
            assert!(Value::from_bytes(t, &data[..width - 1]).is_err());
        }
    }
    for id in [0x41, 0x42, 0x43, 0x44] {
        let t = DataType::try_from(id).unwrap();
        for first in [0, 2, 0xff] {
            let mut wire = vec![first];
            if id >= 0x43 {
                wire.push(if first == 0xff { 0xff } else { 0 });
            }
            if first == 2 {
                wire.extend_from_slice(b"hi");
            }
            assert!(Value::from_bytes(t, &wire).is_ok());
        }
    }
    // Structure containing an array of structures and a scalar sibling.
    let nested = [
        2, 0, 0x48, 0x4c, 2, 0, 1, 0, 0x42, 1, b'a', 1, 0, 0x42, 1, b'b', 0x21, 7, 0,
    ];
    let structure = DataType::try_from(0x4c).unwrap();
    assert_eq!(
        Value::from_bytes(structure, &nested).unwrap().as_bytes(),
        nested
    );
    for end in 0..nested.len() {
        assert!(Value::from_bytes(structure, &nested[..end]).is_err());
    }
    for id in [0x48, 0x50, 0x51] {
        let t = DataType::try_from(id).unwrap();
        for wire in [
            &[0x21, 0, 0][..],
            &[0x21, 0xff, 0xff],
            &[0x20, 2, 0, 7, 8],
            &[0, 0xfe, 0xff],
        ] {
            assert!(Value::from_bytes(t, wire).is_ok());
        }
    }
    assert!(DataType::try_from(0x45).is_err());
}

#[test]
fn reporting_configuration_reuses_the_record_in_a_readback_response() {
    use zigbee_zcl::reporting::{Configuration, ReportingRecord, ReportingResult};
    let type_id = DataType::try_from(0x21).unwrap();
    let record = ReportingRecord {
        id: 0x1234,
        configuration: Configuration::Reported {
            data_type: type_id,
            minimum: 0x0123,
            maximum: 0x4567,
            change: Some(Value::from_bytes(type_id, &[0x9a, 0x78]).unwrap()),
        },
    };
    let body = [0, 0x34, 0x12, 0x21, 0x23, 0x01, 0x67, 0x45, 0x9a, 0x78];
    assert_eq!(
        Payload::decode(frame(0x06, &body)).unwrap(),
        Payload::ConfigureReporting(vec![record.clone()])
    );
    let reply = Payload::ReadReportingConfigurationResponse(vec![ReportingResult::Success(record)]);
    let expected = [&[0][..], &body].concat();
    assert_eq!(reply.encode().unwrap(), expected);
    assert_eq!(Payload::decode(frame(0x09, &expected)).unwrap(), reply);
}
