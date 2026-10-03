use crate::diagnostics::LayerDiagnostics;
use crate::error::{MergeError, WriteError, WriteOptions};
use crate::excellon_format::ExcellonLayerData;
use crate::gerber::GerberLayerData;
use crate::layer::{Layer, LayerData, LayerType, WritePlan};
use crate::{LayerCorners, LayerMerge, LayerRotate, LayerTransform, Pos, error, excellon_format};
use gerber_parser::gerber_types::{Command, CommentContent, FunctionCode, GCode};
use log::{debug, warn};
use std::collections::HashSet;
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
    /// The layer type is inferred from the file name (see
    /// [`LayerType::from_file_name`]): the extension (e.g. `.gtl` → `Top`), or
    /// for `.gbr` files the KiCad layer name (e.g. `-F_Courtyard.gbr`). A
    /// specific X2 `FileFunction` embedded in the Gerber data takes precedence. Files with unrecognised extensions or parse failures are
    /// collected in [`LoadResult::errors`] so the caller can inspect them without
    /// losing the layers that did load correctly.
    pub fn load(data: Vec<(&str, BufReader<&mut dyn Read>)>) -> LoadResult {
        let mut board = Self::empty();
        let mut errors: Vec<(String, error::Error)> = Vec::new();
        for (name, reader) in data {
            let ty = LayerType::from_file_name(name);
            match ty {
                Ok(ty) => {
                    debug!("Parsing layer '{}' as {:?}", name, ty);
                    match Layer::parse(name, ty, reader) {
                        Ok(layer) => board.0.push(layer),
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
                match LayerType::from_file_name(&name) {
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
        match Layer::parse(name.clone(), ty, BufReader::new(file)) {
            Ok(layer) => Ok(layer),
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
    ///
    /// A failed merge leaves the board unchanged.
    pub fn add_layer(&mut self, layer: impl Into<Layer>) -> Result<(), MergeError> {
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
            Ok(())
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

    /// Writes every layer through a writer obtained from `f`.
    ///
    /// Unmodified layers are written as their original bytes. Everything is
    /// checked (names, and completeness of edited layers) before `f` is
    /// called for the first time, so a rejected export opens no writer.
    pub fn write_to<T>(
        &self,
        options: &WriteOptions,
        f: &mut impl FnMut(&Layer) -> std::io::Result<BufWriter<T>>,
    ) -> Result<(), WriteError>
    where
        T: Write,
    {
        let plans = self.plan_write(options)?;
        for (layer, plan) in self.0.iter().zip(plans) {
            let mut writer = f(layer)?;
            layer.write_planned(&mut writer, plan, options)?;
        }
        Ok(())
    }

    /// Runs every check [`write_to`](Self::write_to) performs before
    /// writing, without writing anything.
    pub fn check_write(&self, options: &WriteOptions) -> Result<(), WriteError> {
        self.plan_write(options).map(drop)
    }

    fn plan_write(&self, options: &WriteOptions) -> Result<Vec<WritePlan>, WriteError> {
        self.check_names()?;
        self.0
            .iter()
            .map(|layer| layer.plan_write(options))
            .collect()
    }

    /// Edited layers that would be refused without
    /// [`WriteOptions::allow_incomplete`], with their diagnostics.
    pub fn unresolved_edits(&self) -> Vec<(String, LayerDiagnostics)> {
        self.0
            .iter()
            .filter_map(|layer| Some((layer.name.clone(), layer.unresolved_edit()?)))
            .collect()
    }

    /// Interpretation diagnostics of every layer, by layer name.
    pub fn diagnostics(&self) -> Vec<(String, LayerDiagnostics)> {
        self.0
            .iter()
            .map(|layer| (layer.name.clone(), layer.diagnostics()))
            .collect()
    }

    /// Writes all layers to `path`, creating the directory if necessary.
    ///
    /// Each layer is written to a file named `layer.name` inside `path`. Files
    /// are first written to temporary names and only renamed into place once
    /// every layer was written successfully, so a write failure leaves
    /// existing files untouched (the renames themselves are not atomic as a
    /// group).
    pub fn write_to_folder(&self, path: &Path, options: &WriteOptions) -> Result<(), WriteError> {
        let plans = self.plan_write(options)?;
        fs::create_dir_all(path)?;
        let paths: Vec<(PathBuf, PathBuf)> = self
            .0
            .iter()
            .map(|layer| {
                let tmp = format!(".{}.{}.tmp", layer.name, std::process::id());
                (path.join(tmp), path.join(&layer.name))
            })
            .collect();
        let result = self
            .0
            .iter()
            .zip(plans)
            .zip(&paths)
            .try_for_each(|((layer, plan), (tmp, _))| {
                let mut writer = BufWriter::new(File::create(tmp)?);
                layer.write_planned(&mut writer, plan, options)
            })
            .and_then(|()| {
                for (tmp, target) in &paths {
                    fs::rename(tmp, target)?;
                }
                Ok(())
            });
        if result.is_err() {
            for (tmp, _) in &paths {
                let _ = fs::remove_file(tmp);
            }
        }
        result
    }

    /// Layer names become file names: they must be unique and must not
    /// contain path separators.
    fn check_names(&self) -> Result<(), WriteError> {
        let mut seen = HashSet::new();
        for layer in &self.0 {
            let name = layer.name.as_str();
            if name.is_empty() || name.contains(['/', '\\']) || name == ".." {
                return Err(WriteError::InvalidName(name.to_string()));
            }
            if !seen.insert(name) {
                return Err(WriteError::DuplicateName(name.to_string()));
            }
        }
        Ok(())
    }
}

impl LayerCorners for Board {
    /// Returns the bounding box of the board.
    ///
    /// Computed as the union of all layer corners (Gerber and Excellon drill),
    /// excluding `KeepOut`, `Info`, `SidePlating` and courtyard layers as they
    /// don't represent physical board area.
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
            if matches!(
                layer.ty,
                LayerType::KeepOut
                    | LayerType::Info
                    | LayerType::SidePlating
                    | LayerType::CourtyardTop
                    | LayerType::CourtyardBottom
            ) || matches!(layer.data, LayerData::Info(_))
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
    ///
    /// Every layer pair is checked first, so a failure on any layer leaves
    /// the whole board unchanged.
    fn merge(&mut self, other: &Self) -> Result<(), MergeError> {
        let wrap = |layer: &Layer, e| MergeError::Layer {
            layer: layer.name.clone(),
            source: Box::new(e),
        };
        for layer in &self.0 {
            if let Some(other) = other.get_layer(&layer.ty) {
                layer
                    .data
                    .check_merge(&other.data)
                    .map_err(|e| wrap(layer, e))?;
            }
        }
        for layer in &mut self.0 {
            if let Some(other) = other.get_layer(&layer.ty) {
                layer.data.merge(&other.data).map_err(|e| wrap(layer, e))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gerber::GerberLayerData;

    fn layer(name: &str) -> Layer {
        let mut layer = Layer::from(GerberLayerData::empty(LayerType::Top));
        layer.name = name.to_string();
        layer
    }

    /// Two layers with the same name would overwrite each other; nothing may
    /// be written.
    #[test]
    fn test_write_rejects_duplicate_names() {
        let board = Board(vec![layer("a.gbr"), layer("a.gbr")]);
        let mut opened = 0;
        let result = board.write_to(&WriteOptions::default(), &mut |_| {
            opened += 1;
            Ok(BufWriter::new(Vec::new()))
        });
        assert!(result.is_err());
        assert_eq!(opened, 0);
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("disk full"))
        }
    }

    /// Layer names become file names and must not escape the output folder.
    #[test]
    fn test_write_rejects_path_names() {
        let board = Board(vec![layer("../evil.gbr")]);
        assert!(
            board
                .write_to(&WriteOptions::default(), &mut |_| Ok(BufWriter::new(
                    Vec::new()
                )))
                .is_err()
        );
    }

    /// A failure when buffered bytes are finally written must be reported.
    #[test]
    fn test_write_propagates_flush_error() {
        let board = Board(vec![layer("a.gbr")]);
        let result = board.write_to(&WriteOptions::default(), &mut |_| {
            Ok(BufWriter::new(FailingWriter))
        });
        assert!(result.is_err());
    }

    /// A successful folder write leaves no temporary files behind.
    #[test]
    fn test_write_to_folder_renames_temp_files() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("lge-write-{}", std::process::id()));
        let board = Board(vec![layer("a.gbr"), layer("b.gbr")]);
        board.write_to_folder(&dir, &WriteOptions::default())?;
        let mut names: Vec<_> = fs::read_dir(&dir)?
            .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_, _>>()?;
        names.sort();
        fs::remove_dir_all(&dir)?;
        assert_eq!(names, vec!["a.gbr", "b.gbr"]);
        Ok(())
    }

    const UNPARSEABLE: &str = "%FSLAX46Y46*%\n%MOMM*%\n%ADD10C,0.5*%\nG99*\nD10*\nX0Y0D03*\nM02*\n";

    fn load_one(text: &str) -> Board {
        let mut bytes = text.as_bytes();
        let reader: &mut dyn Read = &mut bytes;
        let result = Board::load(vec![("a.gtl", BufReader::new(reader))]);
        assert!(result.is_ok(), "{:?}", result.errors);
        result.board
    }

    /// Writes every layer into its own buffer after the board-level checks.
    fn write_all(board: &Board, options: &WriteOptions) -> Result<Vec<Vec<u8>>, WriteError> {
        board.check_write(options)?;
        board
            .layers()
            .iter()
            .map(|layer| {
                let mut w = BufWriter::new(Vec::new());
                layer.write_to(&mut w, options)?;
                Ok(w.into_inner().map_err(|e| e.into_error())?)
            })
            .collect()
    }

    /// An unedited layer with content the parser rejected is saved
    /// byte-for-byte.
    #[test]
    fn test_unmodified_layer_written_verbatim() {
        let board = load_one(UNPARSEABLE);
        let layer = &board.layers()[0];
        assert!(!layer.is_modified());
        assert!(!layer.diagnostics().is_complete());
        let out = write_all(&board, &WriteOptions::default()).unwrap();
        assert_eq!(out[0], UNPARSEABLE.as_bytes());
    }

    /// Editing such a layer makes saving fail before any writer is opened,
    /// unless incomplete layers are allowed.
    #[test]
    fn test_edited_incomplete_layer_needs_permission() {
        let mut board = load_one(UNPARSEABLE);
        board.transform(&Pos { x: 1.0, y: 0.0 });
        assert!(board.layers()[0].is_modified());
        let mut opened = 0;
        let err = board
            .write_to(&WriteOptions::default(), &mut |_| {
                opened += 1;
                Ok(BufWriter::new(Vec::new()))
            })
            .unwrap_err();
        assert!(matches!(err, WriteError::Incomplete { .. }), "{err}");
        assert_eq!(opened, 0);
        let unresolved = board.unresolved_edits();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].1.parse_errors.len(), 1);
        let out = write_all(&board, &WriteOptions::default().allow_incomplete(true)).unwrap();
        assert!(!out[0].is_empty());
    }

    /// Any data change counts as a modification; restoring the loaded
    /// state makes the layer unmodified again.
    #[test]
    fn test_is_modified_tracks_state() {
        let mut board = load_one(UNPARSEABLE);
        let layer = &mut board.layers_mut()[0];
        let LayerData::Gerber(g) = &mut layer.data else {
            panic!("expected Gerber");
        };
        let original = g.apertures.clone();
        g.apertures.insert(11, g.apertures[&10].clone());
        assert!(layer.is_modified());
        let LayerData::Gerber(g) = &mut layer.data else {
            unreachable!()
        };
        g.apertures = original;
        assert!(!layer.is_modified());
        layer.ty = LayerType::Bottom;
        assert!(layer.is_modified());
    }
}
