use zigbee_zcl::{Direction, Frame, FrameError, FrameType, Header, foundation::GeneralCommand};

#[test]
fn manufacturer_header_vector_is_little_endian_and_borrows_the_payload() {
    let wire = [0x1c, 0x34, 0x12, 0xa5, 0x0b, 0x42, 0x00];
    let frame = Frame::decode(&wire).unwrap();
    assert_eq!(
        frame.header,
        Header {
            frame_type: FrameType::General,
            manufacturer: Some(0x1234),
            direction: Direction::ServerToClient,
            disable_default_response: true,
            sequence: 0xa5,
            command_id: 0x0b,
        }
    );
    assert_eq!(frame.payload.as_ptr(), wire[5..].as_ptr());
    assert_eq!(frame.payload, &[0x42, 0x00]);
    let mut written = Vec::new();
    frame.encode_into(&mut written);
    assert_eq!(written, wire);
}

#[test]
fn all_control_bytes_and_all_header_truncations() {
    assert_eq!(Frame::decode(&[]), Err(FrameError::TruncatedHeader));
    for control in 0u8..=255 {
        let wire = [control, 0xff, 0xff, 0xff, 0xff, 0xff];
        if control & 0xe0 != 0 || control & 3 > 1 {
            assert_eq!(
                Frame::decode(&wire),
                Err(FrameError::ReservedControl(control))
            );
            continue;
        }
        let len = if control & 4 == 0 { 3 } else { 5 };
        for cut in 0..len {
            assert_eq!(
                Frame::decode(&wire[..cut]),
                Err(FrameError::TruncatedHeader)
            );
        }
        let frame = Frame::decode(&wire).unwrap();
        assert_eq!(frame.payload.len(), wire.len() - len);
        let mut written = Vec::new();
        frame.encode_into(&mut written);
        assert_eq!(written, wire);
    }
}

#[test]
fn command_namespace_is_exhaustive_for_r8() {
    for id in 0u8..=255 {
        let global = [0, 0x55, id];
        let cmd = Frame::decode(&global).unwrap().header.general_command();
        assert_eq!(cmd.map(|v| v as u8), (id <= 0x16).then_some(id));
        let local = [1, 0x55, id];
        assert_eq!(
            Frame::decode(&local).unwrap().header.general_command(),
            None
        );
    }
    assert_eq!(GeneralCommand::ReadAttributesStructured as u8, 0x0e);
    assert_eq!(GeneralCommand::WriteAttributesUndivided as u8, 0x03);
    assert_eq!(
        GeneralCommand::WriteAttributesStructuredResponse as u8,
        0x10
    );
}
