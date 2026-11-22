//! Memory card images, read and edited by the server for the memory card
//! page: the saves on a card, deleting one, copying one to another card.
//! The PlayStation's cards (`ps1`) are 15 blocks with a directory; the PS2's
//! (`ps2`) carry a small FAT file system, a save being a directory.

pub mod ps1;
pub mod ps2;
mod sjis;

use serde::Serialize;

use crate::library::Console;

/// What is on a card.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CardInfo {
    /// False for a PS2 card never formatted (a new one): no saves, and none
    /// can be copied to it until the console formats it.
    pub formatted: bool,
    pub saves: Vec<Save>,
    /// Free space, in bytes, and the card's whole space for saves.
    pub free: u32,
    pub capacity: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Save {
    /// Its name on the card (the PlayStation's file name, the PS2's
    /// directory name): what designates it.
    pub id: String,
    /// The title the console shows, when the save has one.
    pub title: String,
    /// Bytes it takes on the card.
    pub size: u32,
    /// PlayStation saves: the icon's frames, 16×16 RGBA, base64.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub icon: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CardError {
    /// A PS2 card never formatted.
    Unformatted,
    /// The image is not a card of this console, or is damaged.
    Corrupt(&'static str),
    NoSuchSave,
    /// The destination already has a save of that name.
    Exists,
    /// The destination has not enough free space.
    Full,
}

impl std::fmt::Display for CardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unformatted => f.write_str("the memory card is not formatted (the console formats it)"),
            Self::Corrupt(what) => write!(f, "the memory card is damaged: {what}"),
            Self::NoSuchSave => f.write_str("no such save on the memory card"),
            Self::Exists => f.write_str("the destination card already has this save"),
            Self::Full => f.write_str("the destination card has not enough free space"),
        }
    }
}

impl std::error::Error for CardError {}

/// A card as a new one is: the PlayStation's formatted, the PS2's
/// formatted too (the server formats it, as the console's browser would).
pub fn blank(console: Console) -> Vec<u8> {
    match console {
        Console::Ps1 => ps1::formatted(),
        Console::Ps2 => ps2::formatted(),
    }
}

pub fn list(console: Console, image: &[u8]) -> Result<CardInfo, CardError> {
    match console {
        Console::Ps1 => ps1::list(image),
        Console::Ps2 => ps2::list(image),
    }
}

pub fn delete(console: Console, image: &mut [u8], id: &str) -> Result<(), CardError> {
    match console {
        Console::Ps1 => ps1::delete(image, id),
        Console::Ps2 => ps2::delete(image, id),
    }
}

/// Copies save `id` from `from` to `to` (cards of the same console).
pub fn copy(console: Console, from: &[u8], id: &str, to: &mut [u8]) -> Result<(), CardError> {
    match console {
        Console::Ps1 => ps1::copy(from, id, to),
        Console::Ps2 => ps2::copy(from, id, to),
    }
}
