use crate::diagnostics::LayerDiagnostics;
use crate::error::{MergeError, ParseError, WriteError, WriteOptions};
use crate::excellon_format::{ExcellonLayerData, parse_excellon};
use crate::gerber::GerberLayerData;
use crate::{LayerCorners, LayerMerge, LayerRotate, LayerStepAndRepeat, LayerTransform, Pos};
use gerber_parser::gerber_types::{
    Command, CommentContent, ExtendedCode, ExtendedPosition, FileAttribute, FileFunction,
    FunctionCode, GCode, GerberResult, Position, Profile, StandardComment,
};
use log::debug;
use std::fmt::{Display, Formatter};
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::Arc;

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
    /// The file this layer was loaded from, for change detection and
    /// verbatim saving.
    source: Option<LayerSource>,
}

/// Original bytes of a loaded layer. The parsed state is not kept: change
/// detection re-parses the bytes, which keeps memory at one copy.
#[derive(Debug, Clone, PartialEq)]
struct LayerSource {
    bytes: Arc<[u8]>,
    /// The type the bytes were parsed with.
    parsed_as: LayerType,
    /// The type parsing produced.
    ty: LayerType,
}

impl Layer {
    /// A layer that was not loaded from a file (always counts as modified).
    pub fn new(ty: LayerType, name: impl Into<String>, data: impl Into<LayerData>) -> Self {
        Layer {
            ty,
            name: name.into(),
            data: data.into(),
            source: None,
        }
    }

    /// Parses a layer from `reader`, keeping the original bytes.
    pub fn parse<T: Read>(
        name: impl Into<String>,
        ty: LayerType,
        mut reader: BufReader<T>,
    ) -> Result<Self, ParseError> {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(ParseError::ExcellonParseError)?;
        let (parsed_ty, data) = LayerData::parse_bytes(ty, &bytes)?;
        Ok(Layer {
            ty: parsed_ty,
            name: name.into(),
            data,
            source: Some(LayerSource {
                bytes: bytes.into(),
                parsed_as: ty,
                ty: parsed_ty,
            }),
        })
    }

    /// The bytes this layer was loaded from, if any.
    pub fn source_bytes(&self) -> Option<&[u8]> {
        self.source.as_ref().map(|s| &*s.bytes)
    }

    /// Whether the layer differs from what was loaded: its type or any part
    /// of its data (commands, apertures, macros, header) changed. Layers not
    /// loaded from a file are always modified. This re-parses the source,
    /// so callers checking often should cache the answer.
    pub fn is_modified(&self) -> bool {
        let Some(source) = &self.source else {
            return true;
        };
        self.ty != source.ty
            || LayerData::parse_bytes(source.parsed_as, &source.bytes)
                .map_or(true, |(_, original)| original != self.data)
    }

    /// Interpretation diagnostics of the current data.
    pub fn diagnostics(&self) -> LayerDiagnostics {
        LayerDiagnostics::of(&self.data)
    }

    /// Writes the layer: the original bytes when unmodified, otherwise the
    /// regenerated file. Fails with [`WriteError::Incomplete`] for a
    /// modified layer with unresolved diagnostics unless
    /// [`WriteOptions::allow_incomplete`] is set.
    pub fn write_to<T: Write>(
        &self,
        writer: &mut BufWriter<T>,
        options: &WriteOptions,
    ) -> Result<(), WriteError> {
        let plan = self.plan_write(options)?;
        self.write_planned(writer, plan, options)
    }

    /// Decides how [`write_to`](Self::write_to) writes this layer, or why
    /// it must not.
    pub(crate) fn plan_write(&self, options: &WriteOptions) -> Result<WritePlan, WriteError> {
        if self.source.is_some() && !self.is_modified() {
            return Ok(WritePlan::Verbatim);
        }
        if !options.allow_incomplete
            && let Some(diagnostics) = self.incomplete_diagnostics()
        {
            return Err(WriteError::Incomplete {
                layer: self.name.clone(),
                diagnostics: Box::new(diagnostics),
            });
        }
        Ok(WritePlan::Regenerate)
    }

    /// The diagnostics of an edited layer that is not fully understood:
    /// exactly the layers [`write_to`](Self::write_to) refuses without
    /// [`WriteOptions::allow_incomplete`]. `None` when the layer is
    /// unedited or complete.
    pub fn unresolved_edit(&self) -> Option<LayerDiagnostics> {
        if !self.is_modified() {
            return None;
        }
        self.incomplete_diagnostics()
    }

    fn incomplete_diagnostics(&self) -> Option<LayerDiagnostics> {
        let diagnostics = self.diagnostics();
        (!diagnostics.is_complete()).then_some(diagnostics)
    }

    pub(crate) fn write_planned<T: Write>(
        &self,
        writer: &mut BufWriter<T>,
        plan: WritePlan,
        options: &WriteOptions,
    ) -> Result<(), WriteError> {
        match (plan, self.source_bytes()) {
            (WritePlan::Verbatim, Some(bytes)) => {
                writer.write_all(bytes)?;
                writer.flush()?;
            }
            _ => self.data.write_to_with(writer, options)?,
        }
        Ok(())
    }
}

/// How a layer is written, decided once before any file is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WritePlan {
    /// Unmodified: the original bytes.
    Verbatim,
    /// Regenerated from the current data.
    Regenerate,
}

// True when a file's first significant line is the Excellon header marker
// `M48`. CAM tools sometimes emit drill or routing data under a Gerber-style
// extension (e.g. a routing program named `*.gm2`); sniffing the content
// routes those to the Excellon parser instead of failing as Gerber.
fn looks_like_excellon(buf: &[u8]) -> bool {
    for line in buf.split(|&b| b == b'\n') {
        let line = line.trim_ascii();
        if line.is_empty() || line.first() == Some(&b';') {
            continue;
        }
        return line == b"M48";
    }
    false
}

impl LayerData {
    pub fn parse<T>(
        ty: LayerType,
        mut reader: BufReader<T>,
    ) -> Result<(LayerType, LayerData), ParseError>
    where
        T: Read,
    {
        if ty == LayerType::UndefinedGerber {
            let layer = GerberLayerData::from_commands(reader)?;
            return Ok((layer.layer_type, LayerData::Gerber(layer)));
        }
        let mut buf = Vec::new();
        reader
            .read_to_end(&mut buf)
            .map_err(ParseError::ExcellonParseError)?;
        Self::parse_bytes(ty, &buf)
    }

    /// [`parse`](Self::parse) from an in-memory file.
    pub fn parse_bytes(ty: LayerType, buf: &[u8]) -> Result<(LayerType, LayerData), ParseError> {
        if ty == LayerType::UndefinedGerber {
            let layer = GerberLayerData::from_commands(BufReader::new(buf))?;
            return Ok((layer.layer_type, LayerData::Gerber(layer)));
        }
        // `LayerType::Drill` always tries Excellon; for Gerber-style extensions
        // we content-sniff so a misnamed drill/routing file (e.g. `*.gm2`)
        // still parses.
        let try_excellon = ty == LayerType::Drill || looks_like_excellon(buf);
        if try_excellon {
            match parse_excellon(BufReader::new(buf)) {
                Ok(data) => return Ok((ty, LayerData::Excellon(data))),
                Err(err) => debug!("Excellon parse failed for {ty:?}, trying Gerber: {err}"),
            }
        }
        let gerber = GerberLayerData::from_type(ty, BufReader::new(buf))?;
        Ok((gerber.layer_type, LayerData::Gerber(gerber)))
    }

    /// Writes the layer with default [`WriteOptions`].
    pub fn write_to<T>(&self, writer: &mut BufWriter<T>) -> GerberResult<()>
    where
        T: Write,
    {
        self.write_to_with(writer, &WriteOptions::default())
    }

    /// Writes the layer; `options` set the Gerber file attributes.
    pub fn write_to_with<T>(
        &self,
        writer: &mut BufWriter<T>,
        options: &WriteOptions,
    ) -> GerberResult<()>
    where
        T: Write,
    {
        match self {
            LayerData::Gerber(g) => g.write_to_with(writer, options)?,
            LayerData::Excellon(e) => e.write_to(writer)?,
            LayerData::Info(s) => writer.write_all(s.to_string().as_bytes())?,
        }
        // Surface buffered write errors here instead of losing them on drop.
        writer.flush()?;
        Ok(())
    }

    pub fn get_type(&self) -> LayerType {
        match self {
            LayerData::Gerber(layer) => layer.layer_type,
            LayerData::Excellon(_) => LayerType::Drill,
            LayerData::Info(_) => LayerType::Info,
        }
    }

    /// Short name of the data kind, for messages.
    pub fn kind(&self) -> &'static str {
        match self {
            LayerData::Gerber(_) => "Gerber",
            LayerData::Excellon(_) => "Excellon",
            LayerData::Info(_) => "info",
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
    fn check_merge(&self, other: &Self) -> Result<(), MergeError> {
        match (self, other) {
            (LayerData::Excellon(s), LayerData::Excellon(o)) => s.check_merge(o),
            (LayerData::Gerber(s), LayerData::Gerber(o)) => s.check_merge(o),
            (target, other) => Err(MergeError::TypeMismatch {
                target: target.kind(),
                other: other.kind(),
            }),
        }
    }

    fn merge(&mut self, other: &Self) -> Result<(), MergeError> {
        match (self, other) {
            (LayerData::Excellon(s), LayerData::Excellon(o)) => s.merge(o),
            (LayerData::Gerber(s), LayerData::Gerber(o)) => s.merge(o),
            (target, other) => Err(MergeError::TypeMismatch {
                target: target.kind(),
                other: other.kind(),
            }),
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

impl LayerRotate for LayerData {
    fn rotate(&mut self, steps: i32) {
        match self {
            LayerData::Excellon(s) => s.rotate(steps),
            LayerData::Gerber(s) => s.rotate(steps),
            LayerData::Info(_) => {}
        }
    }

    fn rebase(&mut self, steps: i32, offset: &Pos) {
        match self {
            LayerData::Excellon(s) => s.rebase(steps, offset),
            LayerData::Gerber(s) => s.rebase(steps, offset),
            LayerData::Info(_) => {}
        }
    }
}

impl LayerCorners for LayerData {
    fn get_corners(&self) -> (Pos, Pos) {
        match self {
            LayerData::Gerber(g) => g.get_corners(),
            LayerData::Excellon(e) => e.get_corners(),
            // Info layers carry no geometry; callers must exclude them before
            // computing bounds (see `Board::get_corners`).
            LayerData::Info(_) => {
                unreachable!(
                    "Info layers have no geometry; exclude them before calling get_corners"
                )
            }
        }
    }
}

impl LayerCorners for Layer {
    fn get_corners(&self) -> (Pos, Pos) {
        self.data.get_corners()
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
    CourtyardTop,
    CourtyardBottom,
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
            "GBR" | "GBX" | "ART" => Ok(LayerType::UndefinedGerber),
            upper => {
                if let Some(num) = upper.strip_prefix("GL") {
                    let inner_num = num
                        .parse::<i32>()
                        .map_err(|_| "GL must be followed by numbers")?;
                    Ok(LayerType::Inner(inner_num))
                } else if let Some(Ok(ordinal)) = upper.strip_prefix('G').map(str::parse::<u16>) {
                    // KiCad's Protel-style inner copper extension: `.g1` is In1_Cu.
                    Ok(kicad_inner(ordinal.into()))
                } else {
                    Err(format!("Invalid layer type: {}", value))
                }
            }
        }
    }
}

// The Gerber spec has no courtyard file function, so courtyards are written
// as `Other,<value>` with these values and recognised again on load.
const COURTYARD_TOP: &str = "Courtyard_Top";
const COURTYARD_BOTTOM: &str = "Courtyard_Bot";

/// KiCad's inner copper `In<n>_Cu` is copper layer `L<n+1>` in its X2
/// attributes, which is what [`LayerType::Inner`] numbers.
fn kicad_inner(ordinal: i32) -> LayerType {
    LayerType::Inner(ordinal + 1)
}

impl LayerType {
    /// Infers the layer type from a file name.
    ///
    /// The extension is tried first (see [`TryFrom<&str>`]). For generic
    /// Gerber extensions (`.gbr`, …) the KiCad layer name suffix is used when
    /// present (e.g. `board-F_Courtyard.gbr` → `CourtyardTop`), since KiCad
    /// marks some layers, like courtyards, only as `Other,User` in X2.
    pub fn from_file_name(name: &str) -> Result<Self, String> {
        let (stem, ext) = name.rsplit_once('.').unwrap_or((name, name));
        let ty = LayerType::try_from(ext)?;
        if ty != LayerType::UndefinedGerber {
            return Ok(ty);
        }
        Ok(Self::from_kicad_layer_name(stem).unwrap_or(ty))
    }

    /// Maps the KiCad layer name at the end of a plot file stem
    /// (`<project>-<layer>`) to a layer type. Both KiCad 5 (`F_SilkS`,
    /// `F_CrtYd`) and KiCad 6+ (`F_Silkscreen`, `F_Courtyard`) names are
    /// recognised, as is a user layer named `V-Cut`.
    fn from_kicad_layer_name(stem: &str) -> Option<Self> {
        let upper = stem.to_ascii_uppercase();
        // Checked before splitting on `-`, since the name itself contains one.
        if upper == "V-CUT" || upper.ends_with("-V-CUT") {
            return Some(LayerType::VCut);
        }
        let layer = upper.rsplit('-').next()?;
        let ty = match layer {
            "F_CU" => LayerType::Top,
            "B_CU" => LayerType::Bottom,
            "F_MASK" => LayerType::MaskTop,
            "B_MASK" => LayerType::MaskBottom,
            "F_PASTE" => LayerType::PasteTop,
            "B_PASTE" => LayerType::PasteBottom,
            "F_SILKSCREEN" | "F_SILKS" => LayerType::SilkScreenTop,
            "B_SILKSCREEN" | "B_SILKS" => LayerType::SilkScreenBottom,
            // KiCad tags Edge_Cuts as `Profile,NP`, which maps to `Milling`.
            "EDGE_CUTS" => LayerType::Milling,
            "F_COURTYARD" | "F_CRTYD" => LayerType::CourtyardTop,
            "B_COURTYARD" | "B_CRTYD" => LayerType::CourtyardBottom,
            "V_CUT" | "VCUT" => LayerType::VCut,
            inner => {
                let num = inner.strip_prefix("IN")?.strip_suffix("_CU")?;
                kicad_inner(num.parse().ok()?)
            }
        };
        Some(ty)
    }

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
            FileFunction::Other(value) if value.eq_ignore_ascii_case(COURTYARD_TOP) => {
                LayerType::CourtyardTop
            }
            FileFunction::Other(value) if value.eq_ignore_ascii_case(COURTYARD_BOTTOM) => {
                LayerType::CourtyardBottom
            }
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
            LayerType::CourtyardTop => FileFunction::Other(String::from(COURTYARD_TOP)),
            LayerType::CourtyardBottom => FileFunction::Other(String::from(COURTYARD_BOTTOM)),
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
            LayerType::CourtyardTop => write!(f, "Courtyard Top Layer")?,
            LayerType::CourtyardBottom => write!(f, "Courtyard Bottom Layer")?,
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
        Layer::new(
            data.layer_type,
            data.layer_type.file_ending(),
            LayerData::Gerber(data),
        )
    }
}

impl From<ExcellonLayerData> for Layer {
    /// Wraps an [`ExcellonLayerData`] in a [`Layer`].
    ///
    /// The `name` is set to `"drl"`. Override `layer.name` afterwards if a
    /// specific filename is needed.
    fn from(data: ExcellonLayerData) -> Self {
        Layer::new(
            LayerType::Drill,
            LayerType::Drill.file_ending(),
            LayerData::Excellon(data),
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kicad_file_names() {
        let cases = [
            ("mobo-F_Cu.gbr", LayerType::Top),
            ("mobo-B_Cu.gbr", LayerType::Bottom),
            ("mobo-In1_Cu.gbr", LayerType::Inner(2)),
            ("my-board-In2_Cu.gbr", LayerType::Inner(3)),
            ("mobo-F_Mask.gbr", LayerType::MaskTop),
            ("mobo-B_Paste.gbr", LayerType::PasteBottom),
            ("mobo-F_Silkscreen.gbr", LayerType::SilkScreenTop),
            ("mobo-B_SilkS.gbr", LayerType::SilkScreenBottom),
            ("mobo-Edge_Cuts.gbr", LayerType::Milling),
            ("mobo-F_Courtyard.gbr", LayerType::CourtyardTop),
            ("mobo-B_CrtYd.gbr", LayerType::CourtyardBottom),
            ("mobo-User_1.gbr", LayerType::UndefinedGerber),
            ("mobo-V-Cut.gbr", LayerType::VCut),
            ("my-board-V_Cut.gbr", LayerType::VCut),
            ("V-Cut.gbr", LayerType::VCut),
            ("mobo-NoV-Cut.gbr", LayerType::UndefinedGerber),
            ("mobo.g1", LayerType::Inner(2)),
            ("mobo.G2", LayerType::Inner(3)),
            ("mobo.gl2", LayerType::Inner(2)),
            ("mobo.gtl", LayerType::Top),
        ];
        for (name, ty) in cases {
            assert_eq!(LayerType::from_file_name(name), Ok(ty), "{name}");
        }
        assert!(LayerType::from_file_name("mobo-job.gbrjob").is_err());
        assert!(LayerType::from_file_name("mobo.x1").is_err());
    }

    #[test]
    fn courtyard_function_round_trip() {
        for ty in [LayerType::CourtyardTop, LayerType::CourtyardBottom] {
            assert_eq!(LayerType::layer_type(&ty.function()), ty);
        }
    }

    #[test]
    fn kicad_other_user_keeps_file_name_type() {
        let gbr = "%TF.FileFunction,Other,User*%\n%FSLAX46Y46*%\n%MOMM*%\nM02*\n";
        let mut reader: &[u8] = gbr.as_bytes();
        let result = crate::board::Board::load(vec![(
            "mobo-F_Courtyard.gbr",
            BufReader::new(&mut reader as &mut dyn Read),
        )]);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.board.layers()[0].ty, LayerType::CourtyardTop);
    }

    #[test]
    fn kicad_folder() {
        let result = crate::board::Board::from_folder(std::path::Path::new("test/mobo")).unwrap();
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        for layer in result.board.layers() {
            assert_eq!(
                Ok(layer.ty),
                LayerType::from_file_name(&layer.name),
                "{}",
                layer.name
            );
        }
    }
}
