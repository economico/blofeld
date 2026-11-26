use antithesis_sdk::random::AntithesisRng;
use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng};

/// Unified RNG supporting both deterministic local/CI exploration (seeded `StdRng`)
/// and live Antithesis multiverse branching (`AntithesisRng`).
pub enum ExplorerRng {
    Seeded(Box<StdRng>),
    Antithesis(AntithesisRng),
}

impl ExplorerRng {
    pub fn seeded(seed: u64) -> Self {
        Self::Seeded(Box::new(StdRng::seed_from_u64(seed)))
    }

    pub fn antithesis() -> Self {
        Self::Antithesis(AntithesisRng)
    }

    /// Returns true if the current process appears to be running under Antithesis.
    pub fn in_antithesis_env() -> bool {
        std::env::var_os("ANTITHESIS_OUTPUT_DIR").is_some()
    }
}

impl RngCore for ExplorerRng {
    fn next_u32(&mut self) -> u32 {
        match self {
            Self::Seeded(rng) => rng.next_u32(),
            Self::Antithesis(rng) => rng.next_u32(),
        }
    }

    fn next_u64(&mut self) -> u64 {
        match self {
            Self::Seeded(rng) => rng.next_u64(),
            Self::Antithesis(rng) => rng.next_u64(),
        }
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        match self {
            Self::Seeded(rng) => rng.fill_bytes(dest),
            Self::Antithesis(rng) => rng.fill_bytes(dest),
        }
    }
}
