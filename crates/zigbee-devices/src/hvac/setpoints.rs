//! ZCL r8 §6.3.2.2.2. Validate both occupied setpoints in centidegrees.
use super::*;
use zigbee_zcl::attributes::{ReadResult, Status};
use zigbee_zcl::foundation::GeneralCommand;

const CONSTRAINTS: &[u16] = &[0x11, 0x12, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1b];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setpoints {
    heat: Option<i16>,
    cool: Option<i16>,
}

impl Setpoints {
    pub fn new(heat: Option<&Json>, cool: Option<&Json>) -> Result<Self> {
        fn centidegrees(v: &Json) -> Result<i16> {
            let raw = v
                .as_f64()
                .context("expected temperature in degrees Celsius")?
                * 100.;
            ensure!(
                (f64::from(-27315)..=f64::from(i16::MAX)).contains(&raw.round())
                    && (raw - raw.round()).abs() < 1e-8,
                "invalid setpoint or precision"
            );
            Ok(raw.round() as i16)
        }
        ensure!(heat.is_some() || cool.is_some(), "missing setpoint");
        Ok(Self {
            heat: heat.map(centidegrees).transpose()?,
            cool: cool.map(centidegrees).transpose()?,
        })
    }
    pub fn preflight(ep: u8, seq: u8) -> Result<Request> {
        Self::encode(ep, seq, Payload::ReadAttributes(CONSTRAINTS.to_vec()))
    }
    pub fn readback(ep: u8, seq: u8) -> Result<Request> {
        Self::encode(ep, seq, Payload::ReadAttributes(vec![0x11, 0x12]))
    }
    fn encode(ep: u8, seq: u8, payload: Payload<'_>) -> Result<Request> {
        Ok(request(
            ep,
            seq,
            THERMOSTAT,
            FrameType::General,
            payload.command() as u8,
            &payload.encode()?,
        ))
    }
    pub fn write_command(self) -> GeneralCommand {
        if self.heat.is_some() && self.cool.is_some() {
            GeneralCommand::WriteAttributesUndivided
        } else {
            GeneralCommand::WriteAttributes
        }
    }
    pub fn validate(self, ep: u8, seq: u8, rows: &[ReadResult<'_>]) -> Result<Request> {
        ensure!(
            rows.len() == CONSTRAINTS.len(),
            "incomplete thermostat readback"
        );
        let value = |id| {
            rows.iter()
                .find(|r| r.id == id)
                .and_then(|r| match r.result {
                    Status::Success(v) => Some(v),
                    _ => None,
                })
                .context("missing thermostat constraint")
        };
        ensure!(
            matches!(sequence(value(0x1b)?, 5), Some(4 | 5)),
            "operating sequence changed"
        );
        let temp = |id| raw_temperature(value(id)?).context("invalid thermostat temperature");
        let deadband = value(0x19)?;
        let [n @ 10..=25] = *deadband.as_bytes() else {
            bail!("invalid deadband")
        };
        ensure!(deadband.data_type().id() == 0x28, "invalid deadband type");
        let heat = self.heat.unwrap_or(temp(0x12)?);
        let cool = self.cool.unwrap_or(temp(0x11)?);
        ensure!(
            (temp(0x15)?..=temp(0x16)?).contains(&heat)
                && (temp(0x17)?..=temp(0x18)?).contains(&cool),
            "setpoint outside current limits"
        );
        ensure!(
            i32::from(cool) - i32::from(heat) >= i32::from(n) * 10,
            "setpoints violate minimum deadband"
        );
        let h = heat.to_le_bytes();
        let c = cool.to_le_bytes();
        let mut writes = Vec::new();
        for (id, raw, enabled) in [
            (0x12, &h, self.heat.is_some()),
            (0x11, &c, self.cool.is_some()),
        ] {
            if enabled {
                writes.push(AttributeValue {
                    id,
                    value: Value::from_bytes(0x29u8.try_into()?, raw)?,
                });
            }
        }
        let payload = if self.write_command() == GeneralCommand::WriteAttributesUndivided {
            Payload::WriteAttributesUndivided(writes)
        } else {
            Payload::WriteAttributes(writes)
        };
        Self::encode(ep, seq, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn validate_both_setpoints_from_one_complete_read_before_any_write() -> Result<()> {
        let propose = Setpoints::new(Some(&json!(22.5)), None)?;
        let cool = 2450i16.to_le_bytes();
        let heat = 2000i16.to_le_bytes();
        let min = 1000i16.to_le_bytes();
        let max = 3200i16.to_le_bytes();
        let values = [
            (0x11, 0x29, &cool[..]),
            (0x12, 0x29, &heat),
            (0x15, 0x29, &min),
            (0x16, 0x29, &max),
            (0x17, 0x29, &min),
            (0x18, 0x29, &max),
            (0x19, 0x28, &[20]),
            (0x1b, 0x30, &[4]),
        ];
        let mut rows = values.map(|(id, ty, bytes)| ReadResult {
            id,
            result: Status::Success(Value::from_bytes(ty.try_into().unwrap(), bytes).unwrap()),
        });
        assert_eq!(
            propose.validate(1, 2, &rows)?.frame,
            [16, 2, 2, 18, 0, 0x29, 0xca, 8]
        );
        assert!(
            Setpoints::new(Some(&json!(22.51)), None)?
                .validate(1, 2, &rows)
                .is_err()
        );
        assert!(propose.validate(1, 2, &rows[1..]).is_err());
        rows[6].result = Status::Success(Value::from_bytes(0x20u8.try_into()?, &[20])?);
        assert!(propose.validate(1, 2, &rows).is_err());
        rows[6].result = Status::Success(Value::from_bytes(0x28u8.try_into()?, &[9])?);
        assert!(propose.validate(1, 2, &rows).is_err());
        rows[6].result = Status::Success(Value::from_bytes(0x28u8.try_into()?, &[20])?);
        rows[1].id = 0x11;
        assert!(propose.validate(1, 2, &rows).is_err());
        Ok(())
    }
}
