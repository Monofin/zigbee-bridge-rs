//! SONOFF SWV stores both programs as ten binary octets in a ZCL short string.
//! The progress count is supplied by the valve. Writes put zero in its place.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Cycle {
    Timed,
    Quantitative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Program {
    total: u8,
    amount: u32,
    interval: u32,
}

impl Cycle {
    fn amount(self) -> (&'static str, u32) {
        match self {
            Self::Timed => ("irrigation_duration", 86_400),
            Self::Quantitative => ("irrigation_capacity", 6_500),
        }
    }

    pub(super) fn parse(self, input: &Value) -> Result<Program> {
        let fields = input
            .as_object()
            .context("irrigation program requires an object")?;
        ensure!(
            fields.len() == 3,
            "expected three writable irrigation fields"
        );
        let number = |key: &str, max: u32| -> Result<u32> {
            fields
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v <= max)
                .with_context(|| format!("invalid irrigation field {key}"))
        };
        let (key, max) = self.amount();
        Ok(Program {
            total: u8::try_from(number("total_number", 100)?)?,
            amount: number(key, max)?,
            interval: number("irrigation_interval", 86_400)?,
        })
    }

    pub(super) fn state(self, program: Program) -> Value {
        json!({"total_number":program.total, (self.amount().0):program.amount, "irrigation_interval":program.interval})
    }

    pub(super) fn report(self, bytes: &[u8]) -> Option<Value> {
        let [10, current, total, a, b, c, d, e, f, g, h] = *bytes else {
            return None;
        };
        let program = Program {
            total,
            amount: u32::from_be_bytes([a, b, c, d]),
            interval: u32::from_be_bytes([e, f, g, h]),
        };
        let mut state = self.state(program);
        state["current_count"] = json!(current);
        Some(state)
    }
}

impl Program {
    pub(super) fn encode(self) -> Vec<u8> {
        let mut bytes = vec![10, 0, self.total];
        bytes.extend(self.amount.to_be_bytes());
        bytes.extend(self.interval.to_be_bytes());
        bytes
    }
}
