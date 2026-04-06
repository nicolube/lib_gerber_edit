use std::io;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Cannot determine layer type for file: '{0}'")]
    InvalidType(String),
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Failed to parse '{1}': {0}")]
    ParseError(ParseError, String),
}

#[derive(thiserror::Error, Debug)]
pub enum ParseError {
    #[error("Failed to parse Gerber layer: {0}")]
    GerberParseError(#[from] gerber_parser::ParseError),
    #[error("Failed to parse an Excellon layer: {0}")]
    ExcellonParseError(io::Error),
    #[error("Missing coordinate format specification in layer '{0}'")]
    FormatMissing(String),
    #[error("Cannot determine layer type from file attributes")]
    TypeNotFound,
}
