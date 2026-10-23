//! Team-building games where Workforce agents play with humans. The crate is
//! pure: agents, image generation and auth reach it through the [`AgentDriver`],
//! [`ImageGenerator`] and [`TokenVerifier`] ports, which the hub implements.

mod balderdash;
mod caption_battle;
mod driver;
mod game;
mod human_pictionary;
mod manager;
mod protocol;
mod reverse_pictionary;
mod router;
mod spot_the_bot;

pub use driver::{
    AgentDriver, AgentTurnRequest, GeneratedArt, ImageGenerator, PlayRecord, PlayerIdentity,
    Rivalry, RivalryStore, TokenVerifier, VisionGuesser,
};
pub use game::RoomConfig;
pub use manager::GameManager;
pub use protocol::GameKind;
pub use router::{router, GamesState};
