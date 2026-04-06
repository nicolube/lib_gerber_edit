use crate::excellon_format::ExcellonLayerData;
use crate::gerber::GerberLayerData;
use crate::layer::{Layer, LayerData, LayerType};
use crate::{LayerCorners, LayerMerge, LayerTransform, Pos, error, excellon_format};
use gerber_parser::gerber_types::{Command, CommentContent, FunctionCode, GCode, GerberResult};
use std::fs;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
/// A complete PCB stackup: an ordered collection of [`Layer`]s of any type.
///
/// Layers are identified by their [`LayerType`]; each type may appear at most
/// once. Use [`Board::add_layer`] to insert or merge a layer, and
/// [`LayerMerge::merge`] to combine two boards that share the same layer set.
#[derive(Debug, Clone, PartialEq)]
pub struct Board(Vec<Layer>);

impl Board {
    /// Parses a board from a list of `(filename, reader)` pairs.
    ///
    /// The layer type is inferred from the file extension (e.g. `.gtl` → `Top`).
    /// For `.gbr` files the type is read from the `FileAttribute` embedded in
    /// the Gerber data. Returns an error if any file's extension is unrecognised
    /// or its content fails to parse.
    pub fn new(data: Vec<(&str, BufReader<&mut dyn Read>)>) -> crate::Result<Self> {
        let mut result = Vec::new();
        for (name, reader) in data {
            let ty = LayerType::try_from(name.rsplit(".").next().unwrap_or_default());
            match ty {
                Ok(ty) => {
                    let (ty, data) = LayerData::parse(ty, reader)
                        .map_err(|err| error::Error::ParseError(err, name.to_string()))?;
                    result.push(Layer {
                        ty,
                        name: name.to_string(),
                        data,
                    })
                }
                Err(_) => return Err(error::Error::InvalidType(name.to_string())),
            }
        }
        Ok(Self(result))
    }

    /// Creates a board with no layers.
    pub fn empty() -> Self {
        Self(Vec::new())
    }

    /// Appends a comment command to every layer that supports it (Gerber and Excellon).
    pub fn comment(&mut self, txt: String) {
        for layer in self.0.iter_mut() {
            match &mut layer.data {
                LayerData::Gerber(g) => {
                    g.commands
                        .push(Command::FunctionCode(FunctionCode::GCode(GCode::Comment(
                            CommentContent::String(txt.clone()),
                        ))))
                }
                LayerData::Excellon(e) => e
                    .commands
                    .push(Ok(excellon_format::Command::Comment(txt.clone()))),
                LayerData::Info(_) => {}
            }
        }
    }

    /// Loads every file with a recognised Gerber or Excellon extension from `path`.
    ///
    /// Files with unrecognised extensions are silently skipped. Returns an error
    /// if any recognised file fails to open or parse.
    pub fn from_folder(path: &Path) -> crate::Result<Self> {
        let folder = fs::read_dir(path)?;
        let mut files = folder
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let ty = LayerType::try_from(name.rsplit(".").next().unwrap_or_default());
                if matches!(entry.file_type(), Ok(ty) if ty.is_file()) && ty.is_ok() {
                    Some(
                        File::open(entry.path().as_path())
                            .map(|f| (name.clone(), f))
                            .map_err(|e| {
                                error::Error::Io(io::Error::new(
                                    e.kind(),
                                    format!("'{}': {}", name, e),
                                ))
                            }),
                    )
                } else {
                    None
                }
            })
            .collect::<crate::Result<Vec<_>>>()?;
        let reader = files
            .iter_mut()
            .map(|(name, file)| {
                let reader: &mut dyn Read = file;
                (name.as_str(), BufReader::new(reader))
            })
            .collect::<Vec<_>>();
        Self::new(reader)
    }

    /// Returns references to all layers in insertion order.
    pub fn layers(&self) -> Vec<&Layer> {
        self.0.iter().collect()
    }

    /// Returns mutable references to all layers in insertion order.
    pub fn layers_mut(&mut self) -> Vec<&mut Layer> {
        self.0.iter_mut().collect()
    }

    /// Inserts a layer into the board, or merges it into the existing layer of
    /// the same type if one is already present.
    ///
    /// Accepts anything that converts into a [`Layer`] (e.g. [`GerberLayerData`],
    /// [`ExcellonLayerData`](crate::excellon_format::ExcellonLayerData)).
    pub fn add_layer(&mut self, layer: impl Into<Layer>) {
        let layer = layer.into();
        let existing = self.0.iter_mut().find(|e| e.ty == layer.ty);
        if let Some(existing) = existing {
            existing.data.merge(&layer.data)
        } else {
            self.0.push(layer);
        }
    }

    /// Returns the layer with the given type, or `None` if not present.
    pub fn get_layer(&self, ty: &LayerType) -> Option<&Layer> {
        self.0.iter().find(|layer| &layer.ty == ty)
    }

    /// Returns a mutable reference to the layer with the given type, or `None` if not present.
    pub fn get_layer_mut(&mut self, ty: &LayerType) -> Option<&mut Layer> {
        self.0.iter_mut().find(|layer| &layer.ty == ty)
    }

    /// Writes all layers using a caller-supplied writer factory.
    ///
    /// `f` is called once per layer and must return an open `BufWriter`.
    /// Use [`write_to_folder`](Self::write_to_folder) for the common case of
    /// writing files to a directory.
    pub fn write_to<T>(
        &self,
        f: &mut impl FnMut(&Layer) -> std::io::Result<BufWriter<T>>,
    ) -> GerberResult<()>
    where
        T: Write,
    {
        for layer in &self.0 {
            let mut writer = f(layer)?;
            layer.data.write_to(&mut writer)?;
        }
        Ok(())
    }

    /// Writes all layers to `path`, creating the directory if necessary.
    ///
    /// Each layer is written to a file named `layer.name` inside `path`.
    pub fn write_to_folder(&self, path: &Path) -> GerberResult<()> {
        fs::create_dir_all(path)?;
        let mut name_fn = |x: &Layer| {
            let file_path = path.join(&x.name);
            Ok(BufWriter::new(File::create(file_path)?))
        };
        self.write_to(&mut name_fn)
    }
}

impl LayerCorners for Board {
    /// Returns the bounding box of the board.
    ///
    /// Computed as the union of all Gerber layer corners, excluding
    /// `KeepOut`, `Info`, and `SidePlating` layers as they don't represent
    /// physical board area. Excellon layers are also excluded.
    fn get_corners(&self) -> (Pos, Pos) {
        let mut min = Pos {
            x: f64::MAX,
            y: f64::MAX,
        };
        let mut max = Pos {
            x: f64::MIN,
            y: f64::MIN,
        };
        for layer in self.0.iter() {
            // Those layers have no relevance for board size calculations
            if [LayerType::KeepOut, LayerType::Info, LayerType::SidePlating].contains(&layer.ty) {
                continue;
            }
            if let LayerData::Gerber(layer) = &layer.data {
                let (layer_min, layer_max) = layer.get_corners();
                if layer_min.x < min.x {
                    min.x = layer_min.x;
                }
                if layer_max.x > max.x {
                    max.x = layer_max.x;
                }
                if layer_min.y < min.y {
                    min.y = layer_min.y;
                }
                if layer_max.y > max.y {
                    max.y = layer_max.y;
                }
            }
        }
        (min, max)
    }
}

impl LayerTransform for Board {
    /// Translates every layer by `transform` (mm).
    fn transform(&mut self, transform: &Pos) {
        for layer in &mut self.0 {
            layer.data.transform(transform);
        }
    }
}

impl From<Layer> for Board {
    /// Creates a board containing a single layer.
    fn from(layer: Layer) -> Self {
        Self(vec![layer])
    }
}

impl From<Vec<Layer>> for Board {
    /// Creates a board from a pre-built list of layers.
    fn from(layers: Vec<Layer>) -> Self {
        Self(layers)
    }
}

impl From<GerberLayerData> for Board {
    /// Creates a board containing a single Gerber layer.
    fn from(data: GerberLayerData) -> Self {
        Self::from(Layer::from(data))
    }
}

impl From<ExcellonLayerData> for Board {
    /// Creates a board containing a single Excellon drill layer.
    fn from(data: ExcellonLayerData) -> Self {
        Self::from(Layer::from(data))
    }
}

impl LayerMerge for Board {
    /// Merges layers from `other` into the corresponding layers of `self`.
    ///
    /// Only layers whose [`LayerType`] already exists in `self` are updated.
    /// Layer types present in `other` but not in `self` are ignored — use
    /// [`add_layer`](Board::add_layer) to insert a new layer instead.
    fn merge(&mut self, other: &Self) {
        for layer in &mut self.0 {
            if let Some(other) = other.get_layer(&layer.ty) {
                layer.data.merge(&other.data)
            }
        }
    }
}
