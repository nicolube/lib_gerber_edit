use gerber_parser::gerber_types::{FileFunction, GenerationSoftware};
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
    #[error("cannot merge a negative image (%IPNEG*%) with a positive one")]
    ImagePolarityMismatch,
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

/// Why a layer or board could not be written.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum WriteError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("layer name '{0}' is not a plain file name; rename the layer")]
    InvalidName(String),
    #[error("two layers are named '{0}'; writing both would overwrite one; rename one of them")]
    DuplicateName(String),
    #[error("Gerber serialisation failed: {0}")]
    Gerber(#[from] gerber_parser::gerber_types::GerberError),
    /// An edited layer contains content the library did not fully
    /// understand; writing it may drop that content.
    #[error(
        "layer '{layer}' was edited but contains unresolved content, which may be lost on \
         save; fix the issues or set WriteOptions::allow_incomplete:\n{diagnostics}"
    )]
    Incomplete {
        layer: String,
        diagnostics: Box<crate::diagnostics::LayerDiagnostics>,
    },
}

/// Options for writing layers and boards.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct WriteOptions {
    /// Write edited layers even when their diagnostics are not complete.
    pub allow_incomplete: bool,
    /// `TF.CreationDate` of rewritten Gerber files; `None` writes the
    /// current time.
    pub creation_date: Option<chrono::DateTime<chrono::Utc>>,
    /// `TF.GenerationSoftware` of rewritten Gerber files; defaults to this
    /// library and its version, `None` writes none.
    pub generation_software: Option<GenerationSoftware>,
    /// `TF.FileFunction` override for rewritten Gerber files. By default the
    /// source's file function is kept unless the layer type was changed.
    pub file_function: Option<FileFunction>,
}

impl Default for WriteOptions {
    fn default() -> Self {
        WriteOptions {
            allow_incomplete: false,
            creation_date: None,
            generation_software: Some(GenerationSoftware::new(
                "lib_gerber_edit",
                "lib_gerber_edit",
                Some(env!("CARGO_PKG_VERSION")),
            )),
            file_function: None,
        }
    }
}

impl WriteOptions {
    pub fn allow_incomplete(mut self, allow: bool) -> Self {
        self.allow_incomplete = allow;
        self
    }

    pub fn creation_date(mut self, date: Option<chrono::DateTime<chrono::Utc>>) -> Self {
        self.creation_date = date;
        self
    }

    pub fn generation_software(mut self, software: Option<GenerationSoftware>) -> Self {
        self.generation_software = software;
        self
    }

    pub fn file_function(mut self, file_function: Option<FileFunction>) -> Self {
        self.file_function = file_function;
        self
    }
}
