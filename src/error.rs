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
}

/// Why two layers or boards could not be merged. A failed merge leaves the
/// receiver unchanged.
#[derive(thiserror::Error, Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum MergeError {
    #[error("cannot merge {other} data into a {target} layer")]
    TypeMismatch {
        target: &'static str,
        other: &'static str,
    },
    #[error("the {0} ends inside an open region (G36 without G37)")]
    OpenRegion(MergeSide),
    #[error("the {0} ends inside an open aperture block (AB)")]
    OpenBlock(MergeSide),
    #[error("layer '{layer}': {source}")]
    Layer {
        layer: String,
        #[source]
        source: Box<MergeError>,
    },
}

/// Which side of a merge an error refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, derive_more::Display)]
pub enum MergeSide {
    #[display("receiving layer")]
    Receiver,
    #[display("merged layer")]
    Source,
}
