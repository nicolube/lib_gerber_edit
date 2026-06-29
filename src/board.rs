use crate::excellon_format::ExcellonLayerData;
use crate::gerber::GerberLayerData;
use crate::layer::{Layer, LayerData, LayerType};
use crate::{LayerCorners, LayerMerge, LayerRotate, LayerTransform, Pos, error, excellon_format};
use gerber_parser::gerber_types::{Command, CommentContent, FunctionCode, GCode, GerberResult};
use log::{debug, warn};
use std::fs;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// Result of loading a board from a folder.
///
/// Successfully parsed layers are available in [`board`](LoadResult::board).
/// Any files that could not be opened or parsed are collected in
/// [`errors`](LoadResult::errors) as `(filename, error)` pairs, so the caller
/// can inspect failures without losing the layers that did load correctly.
pub struct LoadResult {
    pub board: Board,
    pub errors: Vec<(String, error::Error)>,
}

impl LoadResult {
    /// Returns `true` if every recognised file was loaded without error.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

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
    /// the Gerber data. Files with unrecognised extensions or parse failures are
    /// collected in [`LoadResult::errors`] so the caller can inspect them without
    /// losing the layers that did load correctly.
    pub fn load(data: Vec<(&str, BufReader<&mut dyn Read>)>) -> LoadResult {
        let mut board = Self::empty();
        let mut errors: Vec<(String, error::Error)> = Vec::new();
        for (name, reader) in data {
            let ty = LayerType::try_from(name.rsplit('.').next().unwrap_or_default());
            match ty {
                Ok(ty) => {
                    debug!("Parsing layer '{}' as {:?}", name, ty);
                    match LayerData::parse(ty, reader) {
                        Ok((ty, data)) => board.0.push(Layer {
                            ty,
                            name: name.to_string(),
                            data,
                        }),
                        Err(e) => {
                            warn!("Failed to parse '{}': {}", name, e);
                            errors.push((
                                name.to_string(),
                                error::Error::ParseError(e, name.to_string()),
                            ));
                        }
                    }
                }
                Err(_) => {
                    debug!("Skipping unrecognised file: {}", name);
                    errors.push((
                        name.to_string(),
                        error::Error::InvalidType(name.to_string()),
                    ));
                }
            }
        }
        LoadResult { board, errors }
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
    /// Files with unrecognised extensions are silently skipped.
    /// Returns an `Err` only if the directory itself cannot be read.
    /// Per-file open and parse failures are captured in [`LoadResult::errors`]
    /// so the caller can inspect them without losing the successfully loaded layers.
    ///
    /// When the `parallel` feature is enabled, files are parsed concurrently
    /// using Rayon.
    pub fn from_folder(path: &Path) -> io::Result<LoadResult> {
        let candidates: Vec<(String, LayerType, PathBuf)> = fs::read_dir(path)?
            .filter_map(|e| e.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                if !matches!(entry.file_type(), Ok(ft) if ft.is_file()) {
                    return None;
                }
                let ext = name.rsplit('.').next().unwrap_or_default();
                match LayerType::try_from(ext) {
                    Ok(ty) => Some((name, ty, entry.path())),
                    Err(_) => {
                        debug!("Skipping unrecognised file: {}", name);
                        None
                    }
                }
            })
            .collect();

        #[cfg(feature = "parallel")]
        let results: Vec<Result<Layer, (String, error::Error)>> = candidates
            .into_par_iter()
            .map(Self::parse_candidate)
            .collect();

        #[cfg(not(feature = "parallel"))]
        let results: Vec<Result<Layer, (String, error::Error)>> =
            candidates.into_iter().map(Self::parse_candidate).collect();

        let mut board = Self::empty();
        let mut errors: Vec<(String, error::Error)> = Vec::new();
        for result in results {
            match result {
                Ok(layer) => board.0.push(layer),
                Err(e) => errors.push(e),
            }
        }
        Ok(LoadResult { board, errors })
    }

    fn parse_candidate(
        (name, ty, path): (String, LayerType, PathBuf),
    ) -> Result<Layer, (String, error::Error)> {
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                warn!("Failed to open '{}': {}", name, e);
                return Err((name, error::Error::Io(e)));
            }
        };
        debug!("Parsing layer '{}' as {:?}", name, ty);
        match LayerData::parse(ty, BufReader::new(file)) {
            Ok((ty, data)) => Ok(Layer { ty, name, data }),
            Err(e) => {
                warn!("Failed to parse '{}': {}", name, e);
                Err((name.clone(), error::Error::ParseError(e, name)))
            }
        }
    }

    /// Returns references to all layers in insertion order.
    pub fn layers(&self) -> &[Layer] {
        &self.0
    }

    /// Returns mutable references to all layers in insertion order.
    pub fn layers_mut(&mut self) -> &mut [Layer] {
        &mut self.0
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
            debug!(
                "Merging layer '{}' into existing {:?} layer",
                layer.name, layer.ty
            );
            existing.data.merge(&layer.data)
        } else {
            debug!("Adding new layer '{}' ({:?})", layer.name, layer.ty);
            self.0.push(layer);
        }
    }

    /// Returns `true` if any layer has type [`LayerType::UndefinedGerber`].
    pub fn has_undefined(&self) -> bool {
        self.0.iter().any(|l| l.ty == LayerType::UndefinedGerber)
    }

    /// Returns all layers with type [`LayerType::UndefinedGerber`].
    pub fn get_undefined(&self) -> Vec<&Layer> {
        self.0
            .iter()
            .filter(|l| l.ty == LayerType::UndefinedGerber)
            .collect()
    }

    /// Returns mutable references to all layers with type [`LayerType::UndefinedGerber`].
    pub fn get_undefined_mut(&mut self) -> Vec<&mut Layer> {
        self.0
            .iter_mut()
            .filter(|l| l.ty == LayerType::UndefinedGerber)
            .collect()
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
    /// Computed as the union of all layer corners (Gerber and Excellon drill),
    /// excluding `KeepOut`, `Info`, and `SidePlating` layers as they don't
    /// represent physical board area.
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
            if [LayerType::KeepOut, LayerType::Info, LayerType::SidePlating].contains(&layer.ty)
                || matches!(layer.data, LayerData::Info(_))
            {
                continue;
            }
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

impl LayerRotate for Board {
    fn rotate(&mut self, steps: i32) {
        let steps = steps.rem_euclid(4);
        if steps == 0 {
            return;
        }
        let (min, max) = self.get_corners();
        let cx = (min.x + max.x) * 0.5;
        let cy = (min.y + max.y) * 0.5;
        let (rcx, rcy) = crate::rotate_90(cx, cy, steps);
        self.rebase(
            steps,
            &Pos {
                x: cx - rcx,
                y: cy - rcy,
            },
        );
    }

    fn rebase(&mut self, steps: i32, offset: &Pos) {
        for layer in &mut self.0 {
            layer.data.rebase(steps, offset);
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
