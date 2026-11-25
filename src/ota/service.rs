use super::{Key, MALFORMED, NO_IMAGE, Query, Server, now};
use anyhow::Result;
use zigbee_zcl::cluster::{BlockData, Ota};

pub(crate) enum Response {
    Ota(Ota),
    Default(u8),
}

impl Server {
    pub(crate) fn respond(&mut self, ieee: u64, endpoint: u8, request: Ota) -> Result<Response> {
        let time = now()?;
        Ok(match request {
            Ota::QueryNextImageRequest {
                manufacturer_id,
                image_type,
                current_file_version,
                hardware_version,
                ..
            } => {
                if manufacturer_id == u16::MAX
                    || image_type > 0xffbf
                    || current_file_version == u32::MAX
                    || hardware_version == Some(u16::MAX)
                {
                    return Ok(Response::Default(MALFORMED));
                }
                let query = Query {
                    endpoint,
                    key: Key {
                        manufacturer: manufacturer_id,
                        image_type,
                        version: current_file_version,
                    },
                    hardware: hardware_version,
                };
                let offer = self.query(ieee, query, time);
                Response::Ota(Ota::QueryNextImageResponse {
                    status: if offer.is_some() { 0 } else { NO_IMAGE },
                    manufacturer_id: offer.map(|(k, _)| k.manufacturer),
                    image_type: offer.map(|(k, _)| k.image_type),
                    file_version: offer.map(|(k, _)| k.version),
                    image_size: offer.map(|(_, n)| n),
                })
            }
            Ota::ImageBlockRequest {
                manufacturer_id,
                image_type,
                file_version,
                file_offset,
                max_data_size,
                request_node_address,
                ..
            } => {
                let key = Key {
                    manufacturer: manufacturer_id,
                    image_type,
                    version: file_version,
                };
                if request_node_address.is_some_and(|addr| addr != ieee) {
                    return Ok(Response::Default(MALFORMED));
                }
                let Some(image) = self.offered_image(ieee, endpoint, key, time) else {
                    return Ok(Response::Default(NO_IMAGE));
                };
                let offset = file_offset as usize;
                if max_data_size == 0 || offset >= image.bytes.len() {
                    return Ok(Response::Default(MALFORMED));
                }
                // Leave room for APS security and the ZCL header on a unicast.
                let count = usize::from(max_data_size.min(32)).min(image.bytes.len() - offset);
                Response::Ota(Ota::ImageBlockResponse {
                    status: 0,
                    manufacturer_id: Some(key.manufacturer),
                    image_type: Some(key.image_type),
                    file_version: Some(key.version),
                    file_offset: Some(file_offset),
                    image_data: Some(BlockData(image.bytes[offset..offset + count].to_vec())),
                    current_time: None,
                    request_time: None,
                    minimum_block_period: None,
                })
            }
            Ota::UpgradeEndRequest {
                status,
                manufacturer_id,
                image_type,
                file_version,
            } => {
                let key = Key {
                    manufacturer: manufacturer_id,
                    image_type,
                    version: file_version,
                };
                let offered = self.offered_image(ieee, endpoint, key, time).is_some();
                match status {
                    0 => {
                        let duplicate = self
                            .schedules
                            .get(&ieee)
                            .is_some_and(|s| s.completed_endpoint == Some(endpoint))
                            && self
                                .authorized(ieee, time)
                                .is_some_and(|image| image.key == key);
                        if !duplicate {
                            if !offered {
                                return Ok(Response::Default(NO_IMAGE));
                            }
                            // Save the client's completion before allowing its switch.
                            // A lost response can be regenerated even after a restart.
                            let mut next = self.schedules.clone();
                            next.get_mut(&ieee)
                                .expect("offered schedule")
                                .completed_endpoint = Some(endpoint);
                            self.commit(next)?;
                            self.offered.remove(&ieee);
                        }
                        Response::Ota(Ota::UpgradeEndResponse {
                            manufacturer_id,
                            image_type,
                            file_version,
                            current_time: 0,
                            upgrade_time: 0,
                        })
                    }
                    0x95 | 0x96 | 0x99 => {
                        if offered {
                            self.cancel(ieee)?;
                        }
                        Response::Default(0)
                    }
                    _ => Response::Default(MALFORMED),
                }
            }
            // Optional page and device-specific file services are unimplemented.
            _ => Response::Default(0x81),
        })
    }
}
