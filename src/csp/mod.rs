//! Client side of Clip Studio Paint's Companion Mode.

use serde::{Deserialize, Serialize};

pub mod auth;
pub mod client;
pub mod frame;
pub mod worker;

pub type Rgb = [u8; 3];

/// CSP's colour slots. Transparent holds no value of its own: it is just selected.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Slot {
    Main,
    Sub,
    Transparent,
}
