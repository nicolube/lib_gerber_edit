use std::io;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Failed to derive error for file: {}", 0)]
    InvalidType(String),
    #[error("Io Error: {}", 0)]
    Io(#[from] io::Error),
    #[error("Failed to parse: {}: {}", 1, 0)]
    ParseError(ParseError, String),
}

#[derive(thiserror::Error, Debug)]
pub enum ParseError {
    #[error("Failed to parse a Gerber layer: {} in {}", 0, 1)]
    GerberParseError(#[from] gerber_parser::ParseError),
    #[error("Failed to parse a Excellon layer: {}", 0)]
    ExcellonParseError(io::Error),
    #[error("No coordinate found in: {}", 0)]
    FormatMissing(String),
    #[error("Type not found in file attributes")]
    TypeNotFound,
}
