use crate::error::ParseError;
use crate::excellon_format::{ExcellonLayerData, parse_excellon};
use crate::gerber::GerberLayerData;
use crate::{LayerMerge, LayerStepAndRepeat, LayerTransform, Pos};
use gerber_parser::gerber_types::{
    Command, CommentContent, ExtendedCode, ExtendedPosition, FileAttribute, FileFunction,
    FunctionCode, GCode, GerberResult, Position, Profile, StandardComment,
};
use log::debug;
use std::fmt::{Display, Formatter};
use std::io::{BufReader, BufWriter, Cursor, Read, Write};

#[cfg(feature = "serde")]
use ::serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq)]
pub enum LayerData {
    Gerber(GerberLayerData),
    Excellon(ExcellonLayerData),
    Info(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub ty: LayerType,
    pub name: String,
    pub data: LayerData,
}

impl LayerData {
    pub fn parse<T>(
        ty: LayerType,
        mut reader: BufReader<T>,
    ) -> Result<(LayerType, LayerData), ParseError>
    where
        T: Read,
    {
        Ok(match ty {
            LayerType::Drill => {
                let mut buf = Vec::new();
                reader
                    .read_to_end(&mut buf)
                    .map_err(ParseError::ExcellonParseError)?;
                match parse_excellon(BufReader::new(Cursor::new(&buf))) {
                    Ok(data) => (ty, LayerData::Excellon(data)),
                    Err(excellon_err) => {
                        debug!("Excellon parse failed, trying Gerber: {}", excellon_err);
                        (
                            ty,
                            LayerData::Gerber(GerberLayerData::from_type(
                                ty,
                                BufReader::new(Cursor::new(buf)),
                            )?),
                        )
                    }
                }
            }
            LayerType::UndefinedGerber => {
                let layer = GerberLayerData::from_commands(reader)?;
                (layer.layer_type, LayerData::Gerber(layer))
            }
            _ => (
                ty,
                LayerData::Gerber(GerberLayerData::from_type(ty, reader)?),
            ),
        })
    }

    pub fn write_to<T>(&self, writer: &mut BufWriter<T>) -> GerberResult<()>
    where
        T: Write,
    {
        match self {
            LayerData::Gerber(g) => g.write_to(writer)?,
            LayerData::Excellon(e) => e.write_to(writer)?,
            LayerData::Info(s) => writer.write_all(s.to_string().as_bytes())?,
        }
        Ok(())
    }

    pub fn get_type(&self) -> LayerType {
        match self {
            LayerData::Gerber(layer) => layer.layer_type,
            LayerData::Excellon(_) => LayerType::Drill,
            LayerData::Info(_) => LayerType::Info,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            LayerData::Gerber(layer) => layer.is_empty(),
            LayerData::Excellon(layer) => layer.is_empty(),
            LayerData::Info(data) => data.is_empty(),
        }
    }
}

impl LayerMerge for LayerData {
    fn merge(&mut self, other: &Self) {
        match (self, other) {
            (LayerData::Excellon(s), LayerData::Excellon(o)) => {
                s.merge(o);
            }
            (LayerData::Gerber(s), LayerData::Gerber(o)) => {
                s.merge(o);
            }
            _ => panic!("Cannot merge layers of diffrent type"),
        }
    }
}

impl LayerTransform for LayerData {
    fn transform(&mut self, transform: &Pos) {
        match self {
            LayerData::Excellon(s) => s.transform(transform),
            LayerData::Gerber(s) => s.transform(transform),
            LayerData::Info(_) => {}
        }
    }
}

impl LayerStepAndRepeat for LayerData {
    fn step_and_repeat(&mut self, x_repetitions: u32, y_repetitions: u32, offset: &Pos) {
        match self {
            LayerData::Gerber(g) => {
                g.step_and_repeat(x_repetitions, y_repetitions, offset);
            }
            LayerData::Excellon(e) => {
                e.step_and_repeat(x_repetitions, y_repetitions, offset);
            }
            LayerData::Info(_) => {}
        }
    }
}

/// Layer Type
///
/// All Layers except Drill are usually gerber layers
/// The grill layer is a excellon drill file
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum LayerType {
    Top,
    Bottom,
    Inner(i32),
    PasteTop,
    PasteBottom,
    MaskTop,
    MaskBottom,
    SilkScreenTop,
    SilkScreenBottom,
    Drill,
    Dimensions,
    Milling,
    VCut,
    SidePlating,
    KeepOut,
    Info,
    UndefinedGerber,
}

impl TryFrom<&str> for LayerType {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.to_uppercase().as_str() {
            "GTL" => Ok(LayerType::Top),
            "GBL" => Ok(LayerType::Bottom),
            "GTP" => Ok(LayerType::PasteTop),
            "GBP" => Ok(LayerType::PasteBottom),
            "GTS" => Ok(LayerType::MaskTop),
            "GBS" => Ok(LayerType::MaskBottom),
            "GTO" => Ok(LayerType::SilkScreenTop),
            "GBO" => Ok(LayerType::SilkScreenBottom),
            "DRD" | "DRL" => Ok(LayerType::Drill),
            "GM1" => Ok(LayerType::Dimensions),
            "GM2" => Ok(LayerType::Milling),
            "GVC" => Ok(LayerType::VCut),
            "GSP" => Ok(LayerType::SidePlating),
            "GKO" => Ok(LayerType::KeepOut),
            "GBR" => Ok(LayerType::UndefinedGerber),
            _ if value.starts_with("GL") => {
                let inner_num = value[2..]
                    .parse::<i32>()
                    .map_err(|_| "GL must be followed by numbers")?;
                Ok(LayerType::Inner(inner_num))
            }
            _ => Err(format!("Invalid layer type: {}", value)),
        }
    }
}

impl LayerType {
    /// Searches for a matching FileAttribute in a set of commands
    pub fn from_commands<'a, I: IntoIterator<Item = &'a Command>>(value: I) -> Option<Self> {
        let mut iter = value.into_iter();
        iter.find_map(|c| match c {
            Command::ExtendedCode(ExtendedCode::FileAttribute(FileAttribute::FileFunction(
                file_function,
            ))) => Some(file_function),
            Command::FunctionCode(FunctionCode::GCode(GCode::Comment(
                CommentContent::Standard(StandardComment::FileAttribute(
                    FileAttribute::FileFunction(file_function),
                )),
            ))) => Some(file_function),
            _ => None,
        })
        .map(LayerType::layer_type)
    }
}

impl LayerType {
    /// Returns the default extensional
    pub const fn file_ending(&self) -> &'static str {
        match self {
            LayerType::Info => "txt",
            LayerType::Drill => "drl",
            LayerType::UndefinedGerber => "gbr",
            _ => "gbr",
        }
    }

    /// Converts FileFunction to matching LayerType
    #[allow(clippy::self_named_constructors)]
    pub fn layer_type(file_function: &FileFunction) -> LayerType {
        match file_function {
            FileFunction::Copper {
                layer: _,
                pos: ExtendedPosition::Top,
                copper_type: _,
            } => LayerType::Top,
            FileFunction::Copper {
                layer: _,
                pos: ExtendedPosition::Bottom,
                copper_type: _,
            } => LayerType::Bottom,
            FileFunction::Copper {
                layer,
                pos: ExtendedPosition::Inner,
                copper_type: _,
            } => LayerType::Inner(*layer),
            FileFunction::Paste(Position::Top) => LayerType::PasteTop,
            FileFunction::Paste(Position::Bottom) => LayerType::PasteBottom,
            FileFunction::SolderMask {
                pos: Position::Top,
                index: _,
            } => LayerType::MaskTop,
            FileFunction::SolderMask {
                pos: Position::Bottom,
                index: _,
            } => LayerType::MaskBottom,
            FileFunction::Legend {
                pos: Position::Top,
                index: _,
            } => LayerType::SilkScreenTop,
            FileFunction::Legend {
                pos: Position::Bottom,
                index: _,
            } => LayerType::SilkScreenBottom,
            FileFunction::DrillMap => LayerType::Drill,
            FileFunction::Profile(Some(Profile::Plated)) => LayerType::SidePlating,
            FileFunction::Profile(Some(Profile::NonPlated)) => LayerType::Milling,
            FileFunction::Profile(_) => LayerType::Dimensions,
            FileFunction::VCut(_) => LayerType::VCut,
            FileFunction::KeepOut(_) => LayerType::KeepOut,
            FileFunction::Other(_) => LayerType::Info,
            _ => LayerType::UndefinedGerber,
        }
    }

    /// Converts LayerType to matching FileFunction
    pub fn function(&self) -> FileFunction {
        match self {
            LayerType::Top => FileFunction::Copper {
                layer: 1,
                pos: ExtendedPosition::Top,
                copper_type: None,
            },
            LayerType::Bottom => FileFunction::Copper {
                layer: 99,
                pos: ExtendedPosition::Bottom,
                copper_type: None,
            },
            LayerType::Inner(layer) => FileFunction::Copper {
                layer: *layer,
                pos: ExtendedPosition::Inner,
                copper_type: None,
            },
            LayerType::PasteTop => FileFunction::Paste(Position::Top),
            LayerType::PasteBottom => FileFunction::Paste(Position::Bottom),
            LayerType::MaskTop => FileFunction::SolderMask {
                pos: Position::Top,
                index: None,
            },
            LayerType::MaskBottom => FileFunction::SolderMask {
                pos: Position::Bottom,
                index: None,
            },
            LayerType::SilkScreenTop => FileFunction::Legend {
                pos: Position::Top,
                index: None,
            },
            LayerType::SilkScreenBottom => FileFunction::Legend {
                pos: Position::Bottom,
                index: None,
            },
            LayerType::Drill => FileFunction::DrillMap,
            LayerType::Dimensions => FileFunction::Profile(None),
            LayerType::Milling => FileFunction::Profile(Some(Profile::NonPlated)),
            LayerType::VCut => FileFunction::VCut(None),
            LayerType::SidePlating => FileFunction::Profile(Some(Profile::Plated)),
            LayerType::KeepOut => FileFunction::KeepOut(Position::Top),
            LayerType::Info => FileFunction::Other(String::from("Text")),
            LayerType::UndefinedGerber => FileFunction::Other(String::from("Undefined")),
        }
    }
}

impl Display for LayerType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ", self.file_ending())?;
        match self {
            LayerType::Top => write!(f, "Copper Top Layer")?,
            LayerType::Bottom => write!(f, "Copper Bottom Layer")?,
            LayerType::Inner(num) => write!(f, "Copper Inner Layer {}", num)?,
            LayerType::PasteTop => write!(f, "Paste Top Layer")?,
            LayerType::PasteBottom => write!(f, "Paste Bottom Layer")?,
            LayerType::MaskTop => write!(f, "Mask Top Layer")?,
            LayerType::MaskBottom => write!(f, "Mask Bottom Layer")?,
            LayerType::SilkScreenTop => write!(f, "Silk Screen Top Layer")?,
            LayerType::SilkScreenBottom => write!(f, "Silk Screen Bottom Layer")?,
            LayerType::Drill => write!(f, "Drill Layer")?,
            LayerType::Dimensions => write!(f, "Dimension Layer")?,
            LayerType::Milling => write!(f, "Milling Layer")?,
            LayerType::VCut => write!(f, "V-Cut Layer")?,
            LayerType::SidePlating => write!(f, "Side Plating Layer")?,
            LayerType::KeepOut => write!(f, "Keep Out Layer")?,
            LayerType::Info => write!(f, "Info")?,
            LayerType::UndefinedGerber => write!(f, "Undefined")?,
        };
        Ok(())
    }
}

impl From<Layer> for LayerData {
    fn from(layer: Layer) -> Self {
        layer.data
    }
}

impl From<&Layer> for LayerData {
    fn from(layer: &Layer) -> Self {
        layer.data.clone()
    }
}

impl From<GerberLayerData> for Layer {
    /// Wraps a [`GerberLayerData`] in a [`Layer`].
    ///
    /// The `name` is set to the default file extension for the layer type
    /// (e.g. `"gto"` for `SilkScreenTop`). Override `layer.name` afterwards
    /// if a specific filename is needed.
    fn from(data: GerberLayerData) -> Self {
        Layer {
            name: data.layer_type.file_ending().to_string(),
            ty: data.layer_type,
            data: LayerData::Gerber(data),
        }
    }
}

impl From<ExcellonLayerData> for Layer {
    /// Wraps an [`ExcellonLayerData`] in a [`Layer`].
    ///
    /// The `name` is set to `"drl"`. Override `layer.name` afterwards if a
    /// specific filename is needed.
    fn from(data: ExcellonLayerData) -> Self {
        Layer {
            name: LayerType::Drill.file_ending().to_string(),
            ty: LayerType::Drill,
            data: LayerData::Excellon(data),
        }
    }
}

#[cfg(feature = "serde")]
mod serde {
    use crate::layer::{Layer, LayerData};
    use serde::ser::{Error, SerializeStruct};
    use serde::{Serialize, Serializer};
    use std::io::BufWriter;

    impl Serialize for LayerData {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let mut writer = BufWriter::new(Vec::new());
            self.write_to(&mut writer).map_err(S::Error::custom)?;
            serializer.serialize_str(&String::from_utf8_lossy(
                &writer.into_inner().map_err(S::Error::custom)?,
            ))
        }
    }

    impl Serialize for Layer {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let mut str = serializer.serialize_struct("Layer", 4)?;
            str.serialize_field("name", &self.name)?;
            str.serialize_field("type", &self.ty)?;
            str.serialize_field("file_type", &self.ty.file_ending())?;
            str.serialize_field("data", &self.data)?;
            str.end()
        }
    }
}
