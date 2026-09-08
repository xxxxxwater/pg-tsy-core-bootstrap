use pg_types::Venue;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Ownership {
    Strategy(String),
    Manual,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenuePosition {
    pub venue: Venue,
    pub asset: String,
    pub quantity: String,
    pub ownership: Ownership,
}

pub fn may_open_new_exposure(positions: &[VenuePosition]) -> bool {
    !positions
        .iter()
        .any(|p| p.ownership == Ownership::Unknown)
}
