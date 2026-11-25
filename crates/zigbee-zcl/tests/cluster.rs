use zigbee_zcl::{
    Direction::{ClientToServer, ServerToClient},
    Frame, PayloadError,
    cluster::{Command, Identify, Key, OnOff, Ota},
};

fn decode(cluster: u16, bytes: &[u8]) -> Result<Command, PayloadError> {
    Command::decode(cluster, Frame::decode(bytes).unwrap())
}

fn round_trip(cluster: u16, bytes: &[u8]) -> Command {
    let frame = Frame::decode(bytes).unwrap();
    let value = Command::decode(cluster, frame).unwrap();
    assert_eq!(
        value.key(),
        Key {
            cluster,
            direction: frame.header.direction,
            command: frame.header.command_id
        }
    );
    assert_eq!(value.encode().unwrap(), frame.payload);
    value
}

#[test]
fn dispatch_uses_the_cluster_direction_and_namespace() {
    assert_eq!(
        round_trip(3, &[1, 0, 0, 0x34, 0x12]),
        Command::Identify(Identify::Identify {
            identify_time: 0x1234
        })
    );
    assert_eq!(
        round_trip(3, &[9, 0, 0, 0x34, 0x12]),
        Command::Identify(Identify::IdentifyQueryResponse { timeout: 0x1234 })
    );
    assert_eq!(decode(6, &[0, 0, 0]), Err(PayloadError::NotClusterSpecific));
    assert_eq!(
        decode(6, &[5, 0x34, 0x12, 0, 0]),
        Err(PayloadError::ManufacturerSpecific(0x1234))
    );
    for (cluster, control, direction, cmd) in [
        (6, 9, ServerToClient, 0),
        (3, 1, ClientToServer, 2),
        (0xffff, 1, ClientToServer, 0),
    ] {
        assert_eq!(
            decode(cluster, &[control, 0, cmd]),
            Err(PayloadError::UnsupportedClusterCommand(Key {
                cluster,
                direction,
                command: cmd
            }))
        );
    }
}

#[test]
fn extension_command_keeps_wire_order_and_little_endian_times() {
    let value = round_trip(6, &[1, 0, 0x42, 1, 0x34, 0x12, 0xcd, 0xab]);
    assert_eq!(
        value,
        Command::OnOff(OnOff::OnWithTimedOff {
            on_off_control: 1,
            on_time: 0x1234,
            off_wait_time: 0xabcd
        })
    );
}

#[test]
fn ota_query_responses_omit_image_details_on_failure() {
    for command in [2, 9] {
        round_trip(0x19, &[9, 0, command, 0x98]);
        round_trip(
            0x19,
            &[
                9, 0, command, 0, 0x34, 0x12, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13,
            ],
        );
        assert_eq!(
            decode(0x19, &[9, 0, command, 0]),
            Err(PayloadError::Truncated)
        );
        assert_eq!(
            decode(0x19, &[9, 0, command, 0x98, 0]),
            Err(PayloadError::TrailingBytes)
        );
    }
    let mut value = round_trip(0x19, &[9, 0, 2, 0x98]);
    if let Command::Ota(Ota::QueryNextImageResponse {
        manufacturer_id, ..
    }) = &mut value
    {
        *manufacturer_id = Some(0x1234);
    }
    assert!(matches!(value.encode(), Err(PayloadError::InvalidField(_))));
}

#[test]
fn block_request_decodes_independent_optional_fields() {
    for control in 0..4 {
        let mut bytes = vec![1, 0, 3, control, 1, 0, 2, 0, 3, 0, 0, 0, 4, 0, 0, 0, 64];
        let address = (control & 1 != 0).then_some(0x1234_5678_90ab_cdef_u64);
        let period = (control & 2 != 0).then_some(0x5a3c_u16);
        if let Some(address) = address {
            bytes.extend(address.to_le_bytes());
        }
        if let Some(period) = period {
            bytes.extend(period.to_le_bytes());
        }
        let Command::Ota(Ota::ImageBlockRequest {
            request_node_address,
            minimum_block_period,
            ..
        }) = round_trip(0x19, &bytes)
        else {
            panic!("expected ImageBlockRequest");
        };
        assert_eq!(
            (request_node_address, minimum_block_period),
            (address, period)
        );
    }
}

#[test]
fn image_block_response_uses_counted_data_or_wait_times_or_status_alone() {
    round_trip(0x19, &[9, 0, 5, 0x95]);
    let mut wait = vec![9, 0, 5, 0x97, 1, 0, 0, 0, 2, 0, 0, 0];
    round_trip(0x19, &wait);
    wait.push(0x34);
    assert_eq!(decode(0x19, &wait), Err(PayloadError::Truncated));
    wait.push(0x12);
    round_trip(0x19, &wait);
    let mut data = vec![9, 0, 5, 0, 1, 0, 2, 0, 3, 0, 0, 0, 4, 0, 0, 0, 255];
    data.extend(0..255);
    let mut value = round_trip(0x19, &data);
    let Command::Ota(Ota::ImageBlockResponse {
        image_data: Some(block),
        ..
    }) = &mut value
    else {
        panic!("expected image data");
    };
    block.0.push(0);
    assert!(matches!(value.encode(), Err(PayloadError::InvalidField(_))));
    data.pop();
    assert_eq!(decode(0x19, &data), Err(PayloadError::Truncated));
}

#[test]
fn image_notify_and_hardware_version_follow_the_discriminator() {
    for (kind, extra) in [(0, 0), (1, 2), (2, 4), (3, 8)] {
        let mut bytes = vec![9, 0, 0, kind, 42];
        bytes.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8][..extra]);
        round_trip(0x19, &bytes);
    }
    let mut query = vec![1, 0, 1, 0, 1, 0, 2, 0, 3, 0, 0, 0];
    round_trip(0x19, &query);
    query[3] = 0x81;
    query.extend([4, 0, 0xaa]);
    let frame = Frame::decode(&query).unwrap();
    let (known, extension) = Command::decode_prefix(0x19, frame).unwrap();
    assert_eq!(known.encode().unwrap(), &frame.payload[..11]);
    assert_eq!(extension, &[0xaa]);
    assert_eq!(
        Command::decode(0x19, frame),
        Err(PayloadError::TrailingBytes)
    );
}

// Minimum forms in R8. Each tuple groups consecutive command IDs of the same
// direction. A dash in the command table is represented by None.
#[test]
fn every_reviewed_command_has_a_wire_vector() {
    fn check(cluster: u16, control: u8, first: u8, sizes: &[Option<usize>]) -> usize {
        for (index, size) in sizes.iter().enumerate() {
            let id = first + index as u8;
            let mut bytes = vec![control, 0, id];
            if let Some(size) = size {
                bytes.resize(3 + size, 0);
                round_trip(cluster, &bytes);
                if *size > 0 && !(cluster == 0x300 || (cluster == 8 && id < 8)) {
                    bytes.pop();
                    assert_eq!(decode(cluster, &bytes), Err(PayloadError::Truncated));
                }
            } else {
                assert!(matches!(
                    decode(cluster, &bytes),
                    Err(PayloadError::UnsupportedClusterCommand(_))
                ));
            }
        }
        sizes.iter().flatten().count()
    }
    let s = Some;
    let families: &[(u16, u8, u8, &[Option<usize>])] = &[
        (0, 1, 0, &[s(0)]),
        (3, 1, 0, &[s(2), s(0)]),
        (3, 1, 0x40, &[s(2)]),
        (3, 9, 0, &[s(2)]),
        (4, 1, 0, &[s(3), s(2), s(1), s(2), s(0), s(3)]),
        (4, 9, 0, &[s(3), s(4), s(2), s(3)]),
        (5, 1, 0, &[s(6), s(3), s(3), s(2), s(3), s(3), s(2)]),
        (5, 1, 0x40, &[s(6), s(3), s(7)]),
        (5, 9, 0, &[s(4), s(7), s(4), s(3), s(4), None, s(5)]),
        (5, 9, 0x40, &[s(4), s(7), s(4)]),
        (6, 1, 0, &[s(0), s(0), s(0)]),
        (6, 1, 0x40, &[s(2), s(0), s(5)]),
        (
            8,
            1,
            0,
            &[s(5), s(4), s(6), s(2), s(5), s(4), s(6), s(2), s(2)],
        ),
        (9, 1, 0, &[s(3), s(0), s(0), s(0)]),
        (9, 9, 0, &[s(3), s(8)]),
        (0x15, 1, 0, &[s(3), s(2), s(2), s(2)]),
        (0x15, 9, 0, &[s(1); 4]),
        (
            0x19,
            1,
            1,
            &[s(9), None, s(14), s(18), None, s(9), None, s(18)],
        ),
        (
            0x19,
            9,
            0,
            &[
                s(2),
                None,
                s(13),
                None,
                None,
                s(14),
                None,
                s(16),
                None,
                s(13),
            ],
        ),
        (0x20, 1, 0, &[s(3), s(0), s(4), s(2)]),
        (0x20, 9, 0, &[s(0)]),
        (
            0x102,
            1,
            0,
            &[s(0), s(0), s(0), None, s(2), s(1), None, s(2), s(1)],
        ),
        (0x103, 1, 0, &[s(1), s(0)]),
        (
            0x300,
            1,
            0,
            &[
                s(6),
                s(4),
                s(5),
                s(5),
                s(4),
                s(5),
                s(6),
                s(8),
                s(6),
                s(8),
                s(6),
            ],
        ),
        (
            0x300,
            1,
            0x40,
            &[
                s(7),
                s(5),
                s(7),
                s(7),
                s(9),
                None,
                None,
                s(2),
                None,
                None,
                None,
                s(9),
                s(11),
            ],
        ),
        (0x500, 1, 0, &[s(2), s(0), s(2)]),
        (0x500, 9, 0, &[s(6), s(4), None, None]),
        (0x502, 1, 0, &[s(5), s(1)]),
        (0x600, 1, 0, &[s(1)]),
        (0x600, 9, 0, &[s(9), s(1)]),
        (0x601, 1, 0, &[s(0)]),
    ];
    let total: usize = families
        .iter()
        .map(|&(cluster, dir, first, sizes)| check(cluster, dir, first, sizes))
        .sum();
    // Weekly schedules require a nonzero heat/cool mask even for an empty
    // list. Exercise all Thermostat keys with concrete values.
    let thermostat: &[&[u8]] = &[
        &[1, 0, 0, 2, 0xf6],
        &[1, 0, 1, 0, 0x7f, 1],
        &[1, 0, 2, 0x7f, 3],
        &[1, 0, 3],
        &[1, 0, 4],
        &[9, 0, 0, 0, 0x80, 2],
        &[
            9, 0, 1, 0x68, 1, 0xcd, 0xab, 0x0c, 0xfe, 62, 0x34, 0x08, 0x56, 0x34,
        ],
    ];
    for bytes in thermostat {
        round_trip(0x201, bytes);
        for cut in 3..bytes.len() {
            assert_eq!(decode(0x201, &bytes[..cut]), Err(PayloadError::Truncated));
        }
    }
    assert_eq!(total + thermostat.len(), Command::KEYS.len());
}

#[test]
fn legacy_lighting_options_default_on_receive_and_are_present_on_send() {
    for (cluster, id, required) in [
        (8, 0, &[100, 5, 0][..]),
        (8, 4, &[100, 5, 0]),
        (8, 7, &[]),
        (0x300, 8, &[0xfe, 0xff, 0x2a, 0]),
    ] {
        for (suffix, expected) in [(&[][..], [0, 0]), (&[1], [1, 0]), (&[1, 2], [1, 2])] {
            let bytes = [&[1, 0, id][..], required, suffix].concat();
            let canonical = [required, &expected].concat();
            assert_eq!(
                decode(cluster, &bytes).unwrap().encode().unwrap(),
                canonical
            );
        }
        if !required.is_empty() {
            let bytes = [&[1, 0, id][..], &required[..required.len() - 1]].concat();
            assert_eq!(decode(cluster, &bytes), Err(PayloadError::Truncated));
        }
    }
}

#[test]
fn group_membership_counts_elements_and_names_count_octets() {
    use zigbee_zcl::cluster::{CharacterString, Counted, Groups};
    let req = round_trip(4, &[1, 0, 2, 2, 0x34, 0x12, 0xfe, 0xca]);
    assert_eq!(
        req,
        Command::Groups(Groups::GetGroupMembership {
            group_list: Counted(vec![0x1234, 0xcafe])
        })
    );
    assert_eq!(
        decode(4, &[1, 0, 2, 2, 0x34, 0x12, 0xfe]),
        Err(PayloadError::Truncated)
    );
    assert_eq!(
        decode(4, &[1, 0, 2, 0, 1]),
        Err(PayloadError::TrailingBytes)
    );
    for (prefix, name) in [
        (0, CharacterString::Bytes(vec![])),
        (255, CharacterString::NonValue),
    ] {
        assert_eq!(
            round_trip(4, &[1, 0, 0, 1, 0, prefix]),
            Command::Groups(Groups::AddGroup {
                group_id: 1,
                group_name: name
            })
        );
    }
    assert_eq!(
        round_trip(4, &[1, 0, 0, 1, 0, 2, 0xc3, 0xa9]),
        Command::Groups(Groups::AddGroup {
            group_id: 1,
            group_name: CharacterString::Bytes("é".as_bytes().to_vec())
        })
    );
    let mut max = vec![1, 0, 0, 1, 0, 254];
    max.extend([0x61; 254]);
    round_trip(4, &max);
    let over = Command::Groups(Groups::AddGroup {
        group_id: 1,
        group_name: CharacterString::Bytes(vec![0; 255]),
    });
    assert!(matches!(over.encode(), Err(PayloadError::InvalidField(_))));
    let mut all = vec![1, 0, 2, 255];
    for id in 0u16..255 {
        all.extend(id.to_le_bytes());
    }
    round_trip(4, &all);
    let over = Command::Groups(Groups::GetGroupMembership {
        group_list: Counted(vec![1; 256]),
    });
    assert!(matches!(over.encode(), Err(PayloadError::InvalidField(_))));
}

#[test]
fn failed_views_and_membership_have_different_required_tails() {
    // View Group returns an empty name on failure. Both View Scene forms
    // still identify the requested scene, with no transition, name or sets.
    round_trip(4, &[9, 0, 1, 0x8b, 0x34, 0x12, 0]);
    assert_eq!(
        decode(4, &[9, 0, 1, 0x8b, 0x34, 0x12]),
        Err(PayloadError::Truncated)
    );
    for cmd in [1, 0x41] {
        round_trip(5, &[9, 0, cmd, 0x8b, 0x34, 0x12, 7]);
        assert_eq!(
            decode(5, &[9, 0, cmd, 0x8b, 0x34, 0x12, 7, 0]),
            Err(PayloadError::TrailingBytes)
        );
        round_trip(5, &[9, 0, cmd, 0, 0x34, 0x12, 7, 30, 0, 0, 6, 0, 1, 1]);
    }
    round_trip(5, &[9, 0, 6, 0x85, 0xff, 0x34, 0x12]);
    round_trip(5, &[9, 0, 6, 0, 0xff, 0x34, 0x12, 2, 7, 8]);
    assert_eq!(
        decode(5, &[9, 0, 6, 0, 0xff, 0x34, 0x12]),
        Err(PayloadError::Truncated)
    );
}

#[test]
fn scene_extensions_are_individually_bounded_and_preserve_unknown_clusters() {
    use zigbee_zcl::cluster::{CountedBytes, SceneExtensionFieldSet, Scenes};
    let header = [1, 0, 0, 0x34, 0x12, 7, 30, 0, 1, b'A'];
    let extensions = [6, 0, 1, 1, 0x34, 0xfe, 3, 0x10, 0x20, 0x30, 8, 0, 0];
    let bytes = [&header[..], &extensions].concat();
    let Command::Scenes(Scenes::AddScene {
        extension_field_sets,
        ..
    }) = round_trip(5, &bytes)
    else {
        panic!("expected Add Scene");
    };
    assert_eq!(
        extension_field_sets,
        [
            SceneExtensionFieldSet {
                cluster_id: 6,
                value: CountedBytes(vec![1])
            },
            SceneExtensionFieldSet {
                cluster_id: 0xfe34,
                value: CountedBytes(vec![0x10, 0x20, 0x30])
            },
            SceneExtensionFieldSet {
                cluster_id: 8,
                value: CountedBytes(vec![])
            },
        ]
    );
    for end in 0..=extensions.len() {
        let result = decode(5, &bytes[..header.len() + end]);
        if [0, 4, 10, 13].contains(&end) {
            assert!(result.is_ok());
        } else {
            assert_eq!(result, Err(PayloadError::Truncated));
        }
    }
}

#[test]
fn recall_scene_preserves_absent_explicit_and_stored_transition_times() {
    use zigbee_zcl::cluster::Scenes;
    for transition_time in [None, Some(0), Some(0x1234_u16), Some(0xffff)] {
        let mut bytes = vec![1, 0, 5, 0x34, 0x12, 7];
        if let Some(time) = transition_time {
            bytes.extend(time.to_le_bytes());
        }
        assert_eq!(
            round_trip(5, &bytes),
            Command::Scenes(Scenes::RecallScene {
                group_id: 0x1234,
                scene_id: 7,
                transition_time
            })
        );
    }
    assert_eq!(
        decode(5, &[1, 0, 5, 0, 0, 0, 0]),
        Err(PayloadError::Truncated)
    );
}

#[test]
fn weekly_schedules_share_mode_across_transitions_in_both_directions() {
    use zigbee_zcl::cluster::{ScheduleMode, ScheduleTransition, Thermostat, WeeklySchedule};
    for (mode, values, heat, cool) in [
        (ScheduleMode::HEAT, &[0x0c, 0xfe][..], Some(-500), None),
        (ScheduleMode::COOL, &[0x34, 0x08], None, Some(2100)),
        (
            ScheduleMode::HEAT_AND_COOL,
            &[0x0c, 0xfe, 0x34, 0x08],
            Some(-500),
            Some(2100),
        ),
    ] {
        let schedule = WeeklySchedule {
            days: 0x81,
            mode,
            transitions: vec![
                ScheduleTransition {
                    minutes_since_midnight: 360,
                    heat_setpoint: heat,
                    cool_setpoint: cool,
                },
                ScheduleTransition {
                    minutes_since_midnight: 1410,
                    heat_setpoint: heat,
                    cool_setpoint: cool,
                },
            ],
        };
        for (header, expected) in [
            (
                [1, 0, 1],
                Thermostat::SetWeeklySchedule {
                    schedule: schedule.clone(),
                },
            ),
            (
                [9, 0, 0],
                Thermostat::CurrentWeeklySchedule {
                    schedule: schedule.clone(),
                },
            ),
        ] {
            let bytes = [
                &header[..],
                &[2, 0x81, mode.bits(), 0x68, 1],
                values,
                &[0x82, 5],
                values,
            ]
            .concat();
            assert_eq!(round_trip(0x201, &bytes), Command::Thermostat(expected));
            for cut in 3..bytes.len() {
                assert_eq!(decode(0x201, &bytes[..cut]), Err(PayloadError::Truncated));
            }
        }
    }
}

#[test]
fn thermostat_rejects_inconsistent_modes_and_oversized_fragments() {
    use zigbee_zcl::cluster::{ScheduleMode, Thermostat, WeeklySchedule};
    let mut ten = vec![1, 0, 1, 10, 0x7f, 1];
    ten.extend([0; 40]);
    let mut cmd = round_trip(0x201, &ten);
    let Command::Thermostat(Thermostat::SetWeeklySchedule { schedule }) = &mut cmd else {
        panic!("expected schedule");
    };
    schedule.transitions.push(schedule.transitions[0].clone());
    assert!(matches!(cmd.encode(), Err(PayloadError::InvalidField(_))));
    ten[3] = 11;
    ten.extend([0; 4]);
    assert!(matches!(
        decode(0x201, &ten),
        Err(PayloadError::InvalidField(_))
    ));
    assert!(matches!(
        decode(0x201, &[1, 0, 2, 0x7f, 0]),
        Err(PayloadError::InvalidField(_))
    ));
    assert!(matches!(
        decode(0x201, &[9, 0, 0, 0, 0x7f, 0x80]),
        Err(PayloadError::InvalidField(_))
    ));
    let mut value = round_trip(0x201, &[1, 0, 1, 1, 0xff, 0x81, 0, 0, 0xd0, 7]);
    if let Command::Thermostat(Thermostat::SetWeeklySchedule {
        schedule: WeeklySchedule { mode, .. },
    }) = &mut value
    {
        *mode = ScheduleMode::COOL;
    }
    assert!(matches!(value.encode(), Err(PayloadError::InvalidField(_))));
    assert_eq!(
        decode(0x201, &[1, 0, 1, 0, 1, 1, 0]),
        Err(PayloadError::TrailingBytes)
    );
}

#[test]
fn ias_zone_notification_preserves_delay_in_quarter_seconds() {
    use zigbee_zcl::cluster::IasZone;
    assert_eq!(
        round_trip(0x500, &[9, 0, 0, 0x21, 0x02, 0, 0x53, 0x34, 0x12]),
        Command::IasZone(IasZone::ZoneStatusChangeNotification {
            zone_status: 0x0221,
            extended_status: 0,
            zone_id: 0x53,
            delay: 0x1234
        })
    );
}

#[test]
fn tunnel_addresses_have_lengths_and_bacnet_uses_the_remaining_octets() {
    use zigbee_zcl::cluster::{BacnetProtocolTunnel, GenericTunnel, OctetString};
    let address = || OctetString::Bytes(vec![0xfe, 0, 0x81]);
    assert_eq!(
        round_trip(0x0600, &[1, 0, 0, 3, 0xfe, 0, 0x81]),
        Command::GenericTunnel(GenericTunnel::MatchProtocolAddress {
            protocol_address: address()
        })
    );
    assert_eq!(
        round_trip(0x0600, &[9, 0, 1, 3, 0xfe, 0, 0x81]),
        Command::GenericTunnel(GenericTunnel::AdvertiseProtocolAddress {
            protocol_address: address()
        })
    );
    let response = [9, 0x7e, 0, 8, 7, 6, 5, 4, 3, 2, 1, 3, 0xfe, 0, 0x81];
    assert_eq!(
        round_trip(0x0600, &response),
        Command::GenericTunnel(GenericTunnel::MatchProtocolAddressResponse {
            device_ieee_address: 0x0102_0304_0506_0708,
            protocol_address: address()
        })
    );
    for cut in 3..response.len() {
        assert_eq!(
            decode(0x0600, &response[..cut]),
            Err(PayloadError::Truncated)
        );
    }
    assert_eq!(
        round_trip(0x0600, &[1, 0, 0, 0xff]),
        Command::GenericTunnel(GenericTunnel::MatchProtocolAddress {
            protocol_address: OctetString::NonValue
        })
    );
    assert!(
        Command::GenericTunnel(GenericTunnel::MatchProtocolAddress {
            protocol_address: OctetString::Bytes(vec![0; 255]),
        })
        .encode()
        .is_err()
    );
    assert_eq!(
        round_trip(0x0601, &[1, 3, 0, 1, 0x20, 0xff, 0, 7, 0x0b]),
        Command::BacnetProtocolTunnel(BacnetProtocolTunnel::TransferNpdu {
            npdu: vec![1, 0x20, 0xff, 0, 7, 0x0b]
        })
    );
    assert!(matches!(
        decode(0x0601, &[9, 0, 0, 1, 0]),
        Err(PayloadError::UnsupportedClusterCommand(_))
    ));
}
