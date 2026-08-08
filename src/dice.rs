use std::{
    fmt,
    io::Read,
    sync::{Arc, OnceLock},
    time::Duration,
};

use sha2::{Digest, Sha256};
use thiserror::Error;

/// Keeps the fully enumerated Discord response below the platform message limit.
pub const MAX_DICE: u16 = 100;
pub const MAX_SIDES: u32 = 1_000_000;
pub const MAX_MODIFIER: i64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiceCount(u16);

impl DiceCount {
    pub fn get(self) -> u16 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DieSides(u32);

impl DieSides {
    pub fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiceModifier(i64);

impl DiceModifier {
    pub fn get(self) -> i64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiceExpression {
    count: DiceCount,
    sides: DieSides,
    modifier: DiceModifier,
}

impl DiceExpression {
    pub fn parse(input: &str) -> Result<Self, DiceParseError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(DiceParseError::Empty);
        }
        if !input.is_ascii() || input.bytes().any(|byte| byte.is_ascii_whitespace()) {
            return Err(DiceParseError::InvalidNotation);
        }

        let (count_text, remainder) = input
            .split_once(['d', 'D'])
            .ok_or(DiceParseError::InvalidNotation)?;
        if remainder.is_empty() || remainder.contains(['d', 'D']) {
            return Err(DiceParseError::InvalidNotation);
        }

        let count = if count_text.is_empty() {
            1
        } else {
            parse_bounded_unsigned(count_text, u64::from(MAX_DICE), "dice count")? as u16
        };
        if count == 0 {
            return Err(DiceParseError::OutOfRange {
                field: "dice count",
                minimum: 1,
                maximum: u64::from(MAX_DICE),
            });
        }

        let modifier_index = remainder
            .char_indices()
            .skip(1)
            .find_map(|(index, character)| matches!(character, '+' | '-').then_some(index));
        let (sides_text, modifier_text) = modifier_index.map_or((remainder, None), |index| {
            (&remainder[..index], Some(&remainder[index..]))
        });

        let sides = parse_bounded_unsigned(sides_text, u64::from(MAX_SIDES), "sides")? as u32;
        if sides < 2 {
            return Err(DiceParseError::OutOfRange {
                field: "sides",
                minimum: 2,
                maximum: u64::from(MAX_SIDES),
            });
        }

        let modifier = modifier_text.map_or(Ok(0), parse_modifier)?;
        Ok(Self {
            count: DiceCount(count),
            sides: DieSides(sides),
            modifier: DiceModifier(modifier),
        })
    }

    pub fn count(&self) -> DiceCount {
        self.count
    }

    pub fn sides(&self) -> DieSides {
        self.sides
    }

    pub fn modifier(&self) -> DiceModifier {
        self.modifier
    }
}

impl fmt::Display for DiceExpression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}d{}", self.count.0, self.sides.0)?;
        match self.modifier.0 {
            modifier if modifier > 0 => write!(formatter, "+{modifier}"),
            modifier if modifier < 0 => write!(formatter, "{modifier}"),
            _ => Ok(()),
        }
    }
}

fn parse_bounded_unsigned(
    input: &str,
    maximum: u64,
    field: &'static str,
) -> Result<u64, DiceParseError> {
    if input.is_empty() || !input.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DiceParseError::InvalidNotation);
    }
    let normalized = input.trim_start_matches('0');
    if normalized.is_empty() {
        return Ok(0);
    }
    if normalized.len() > maximum.to_string().len() {
        return Err(DiceParseError::OutOfRange {
            field,
            minimum: 1,
            maximum,
        });
    }
    let value = normalized
        .parse::<u64>()
        .map_err(|_| DiceParseError::InvalidNotation)?;
    if value > maximum {
        return Err(DiceParseError::OutOfRange {
            field,
            minimum: 1,
            maximum,
        });
    }
    Ok(value)
}

fn parse_modifier(input: &str) -> Result<i64, DiceParseError> {
    let (sign, digits) = match input.as_bytes().first() {
        Some(b'+') => (1, &input[1..]),
        Some(b'-') => (-1, &input[1..]),
        _ => return Err(DiceParseError::InvalidNotation),
    };
    let magnitude = parse_bounded_unsigned(digits, MAX_MODIFIER as u64, "modifier")? as i64;
    Ok(sign * magnitude)
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DiceParseError {
    #[error("dice expression is empty")]
    Empty,
    #[error("use dice notation NdS, optionally followed by +M or -M (for example: 2d20+5)")]
    InvalidNotation,
    #[error("{field} must be between {minimum} and {maximum}")]
    OutOfRange {
        field: &'static str,
        minimum: u64,
        maximum: u64,
    },
}

pub const ENTROPY_SEED_BYTES: usize = 32;
const MAX_HARDWARE_SENSOR_FILES: usize = 64;
const MAX_HARDWARE_SENSOR_CANDIDATES: usize = 256;
const MAX_HARDWARE_SENSOR_VALUE_BYTES: u64 = 64;
const MAX_HARDWARE_SENSOR_STATE_BYTES: usize = 8 * 1_024;
const HARDWARE_SENSOR_BUDGET: Duration = Duration::from_millis(50);

pub struct GeneratedSeed {
    pub bytes: [u8; ENTROPY_SEED_BYTES],
    pub provenance: EntropyProvenance,
}

pub trait SeedSource {
    fn generate_seed(&mut self) -> Result<GeneratedSeed, RollError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntropyProvenance {
    OsCsprng,
    OsCsprngHardware,
    #[cfg(test)]
    TestSequence,
}

impl fmt::Display for EntropyProvenance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OsCsprng => formatter.write_str("os-csprng"),
            Self::OsCsprngHardware => formatter.write_str("os-csprng+hwmon"),
            #[cfg(test)]
            Self::TestSequence => formatter.write_str("test-sequence"),
        }
    }
}

pub struct OsEntropy {
    sensor_state: Vec<u8>,
}

impl OsEntropy {
    pub async fn collect() -> Self {
        static SENSOR_PERMIT: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
        let semaphore =
            Arc::clone(SENSOR_PERMIT.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1))));
        let Ok(permit) = semaphore.try_acquire_owned() else {
            return Self {
                sensor_state: Vec::new(),
            };
        };
        let read = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            read_hardware_sensor_state()
        });
        let sensor_state = match tokio::time::timeout(HARDWARE_SENSOR_BUDGET, read).await {
            Ok(Ok(state)) => state,
            Ok(Err(_)) | Err(_) => Vec::new(),
        };
        Self { sensor_state }
    }
}

impl SeedSource for OsEntropy {
    fn generate_seed(&mut self) -> Result<GeneratedSeed, RollError> {
        let mut bytes = [0_u8; ENTROPY_SEED_BYTES];
        getrandom::fill(&mut bytes).map_err(|error| RollError::Entropy(error.to_string()))?;
        Ok(mix_seed_with_sensor_state(bytes, &self.sensor_state))
    }
}

fn mix_seed_with_sensor_state(
    mut bytes: [u8; ENTROPY_SEED_BYTES],
    sensor_state: &[u8],
) -> GeneratedSeed {
    let provenance = if sensor_state.is_empty() {
        EntropyProvenance::OsCsprng
    } else {
        let digest = Sha256::new()
            .chain_update(bytes)
            .chain_update(sensor_state)
            .finalize();
        bytes.copy_from_slice(&digest);
        EntropyProvenance::OsCsprngHardware
    };
    GeneratedSeed { bytes, provenance }
}

fn read_hardware_sensor_state() -> Vec<u8> {
    let Ok(devices) = std::fs::read_dir("/sys/class/hwmon") else {
        return Vec::new();
    };
    let mut paths = devices
        .flatten()
        .flat_map(|device| {
            std::fs::read_dir(device.path())
                .into_iter()
                .flatten()
                .flatten()
        })
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    (name.starts_with("temp") || name.starts_with("in")) && name.ends_with("_input")
                })
        })
        .take(MAX_HARDWARE_SENSOR_CANDIDATES)
        .collect::<Vec<_>>();
    paths.sort();
    paths.truncate(MAX_HARDWARE_SENSOR_FILES);
    let mut state = Vec::new();
    for path in paths {
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let Some(value) = read_bounded_sensor_value(file) else {
            continue;
        };
        let Ok(value) = std::str::from_utf8(&value) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-')
        {
            continue;
        }
        let addition = path.as_os_str().as_encoded_bytes().len() + value.len() + 2;
        if state.len() + addition > MAX_HARDWARE_SENSOR_STATE_BYTES {
            break;
        }
        state.extend_from_slice(path.as_os_str().as_encoded_bytes());
        state.push(0);
        state.extend_from_slice(value.as_bytes());
        state.push(0xff);
    }
    state
}

fn read_bounded_sensor_value(reader: impl Read) -> Option<Vec<u8>> {
    let mut value = Vec::new();
    reader
        .take(MAX_HARDWARE_SENSOR_VALUE_BYTES + 1)
        .read_to_end(&mut value)
        .ok()?;
    (value.len() as u64 <= MAX_HARDWARE_SENSOR_VALUE_BYTES).then_some(value)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollResult {
    pub expression: DiceExpression,
    pub dice: Vec<u32>,
    pub total: i64,
    pub entropy_source: EntropyProvenance,
}

pub(crate) fn render_modifier(modifier: i64) -> String {
    match modifier {
        value if value > 0 => format!(" + {value}"),
        value if value < 0 => format!(" - {}", value.unsigned_abs()),
        _ => String::new(),
    }
}

pub fn roll_from_seed(
    expression: DiceExpression,
    seed: [u8; ENTROPY_SEED_BYTES],
    entropy_source: EntropyProvenance,
) -> RollResult {
    let mut entropy = SeededEntropy::new(seed);
    let mut dice = Vec::with_capacity(usize::from(expression.count.0));
    for _ in 0..expression.count.0 {
        dice.push(sample_face(expression.sides.0, &mut entropy));
    }
    let total = dice.iter().map(|&face| i64::from(face)).sum::<i64>() + expression.modifier.0;
    RollResult {
        expression,
        dice,
        total,
        entropy_source,
    }
}

struct SeededEntropy {
    seed: [u8; ENTROPY_SEED_BYTES],
    counter: u64,
}

impl SeededEntropy {
    fn new(seed: [u8; ENTROPY_SEED_BYTES]) -> Self {
        Self { seed, counter: 0 }
    }

    fn next_u64(&mut self) -> u64 {
        let digest = Sha256::new()
            .chain_update(self.seed)
            .chain_update(self.counter.to_le_bytes())
            .finalize();
        self.counter = self.counter.checked_add(1).expect("roll counter overflow");
        u64::from_le_bytes(digest[..8].try_into().expect("SHA-256 prefix is 8 bytes"))
    }
}

fn sample_face(sides: u32, entropy: &mut SeededEntropy) -> u32 {
    let bound = u64::from(sides);
    let acceptance_limit = u64::MAX - (u64::MAX % bound);
    loop {
        let sample = entropy.next_u64();
        if sample < acceptance_limit {
            return (sample % bound) as u32 + 1;
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RollError {
    #[error("operating-system entropy unavailable: {0}")]
    Entropy(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_and_canonicalizes_supported_notation() {
        let cases = [
            ("d6", "1d6"),
            ("2D20", "2d20"),
            ("08d006+0003", "8d6+3"),
            ("4d6-1", "4d6-1"),
            ("100d6", "100d6"),
        ];
        for (input, expected) in cases {
            assert_eq!(DiceExpression::parse(input).unwrap().to_string(), expected);
        }
    }

    #[test]
    fn rejects_ambiguous_and_out_of_range_notation_before_arithmetic() {
        for input in [
            "",
            "0d6",
            "1d1",
            "101d6",
            "1d1000001",
            "1d6+1000001",
            "1d6+18446744073709551616",
            "1d6 1d8",
            "1dd6",
            "1d6+",
        ] {
            assert!(DiceExpression::parse(input).is_err(), "accepted {input}");
        }
    }

    #[test]
    fn seeded_roll_is_deterministic_and_faces_are_in_range() {
        let expression = DiceExpression::parse("2d6+3").unwrap();
        let first = roll_from_seed(expression.clone(), [7; 32], EntropyProvenance::TestSequence);
        let second = roll_from_seed(expression, [7; 32], EntropyProvenance::TestSequence);
        assert_eq!(first, second);
        assert!(first.dice.iter().all(|face| (1..=6).contains(face)));
        assert_eq!(
            first.total,
            first.dice.iter().map(|face| i64::from(*face)).sum::<i64>() + 3
        );
    }

    #[test]
    fn roll_result_preserves_typed_provenance() {
        let expression = DiceExpression::parse("d6").unwrap();
        let result = roll_from_seed(expression, [3; 32], EntropyProvenance::TestSequence);
        assert_eq!(result.dice.len(), 1);
        assert_eq!(result.entropy_source, EntropyProvenance::TestSequence);
    }

    #[test]
    fn hardware_state_is_mixed_only_when_available_and_changes_provenance() {
        let fallback = mix_seed_with_sensor_state([4; 32], &[]);
        assert_eq!(fallback.bytes, [4; 32]);
        assert_eq!(fallback.provenance, EntropyProvenance::OsCsprng);

        let mixed = mix_seed_with_sensor_state([4; 32], b"temp1=42000");
        assert_ne!(mixed.bytes, [4; 32]);
        assert_eq!(mixed.provenance, EntropyProvenance::OsCsprngHardware);
        assert_eq!(
            mixed.bytes,
            mix_seed_with_sensor_state([4; 32], b"temp1=42000").bytes
        );
    }

    #[test]
    fn hardware_sensor_values_are_byte_bounded() {
        assert_eq!(
            read_bounded_sensor_value(std::io::Cursor::new(vec![b'1'; 64])).unwrap(),
            vec![b'1'; 64]
        );
        assert!(read_bounded_sensor_value(std::io::Cursor::new(vec![b'1'; 65])).is_none());
    }
}
