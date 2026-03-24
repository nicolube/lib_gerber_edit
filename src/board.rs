use crate::layer::{Layer, LayerData, LayerType};
use crate::{LayerCorners, LayerMerge, LayerTransform, Pos, error, excellon_format};
use gerber_parser::gerber_types::{Command, CommentContent, FunctionCode, GCode, GerberResult};
use std::fs;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
/// Board, a collection of layers of any type
#[derive(Debug, Clone, PartialEq)]
pub struct Board(Vec<Layer>);

impl Board {
    /// Loads board from a Vec of layers file-name and a reader of the files data
    ///
    /// Layer type will be evaluated from file extension.
    /// If gerber layer type cannot be evaluated by file extension, type will be read from
    /// gerber FileAttribute.
    pub fn new(data: Vec<(&str, BufReader<&mut dyn Read>)>) -> crate::Result<Self> {
        let mut result = Vec::new();
        for (name, reader) in data {
            let ty = LayerType::try_from(name.rsplit(".").next().unwrap());
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

    /// Returns an empty layer
    pub fn empty() -> Self {
        Self(Vec::new())
    }

    /// Adds a comment to all layers
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

    /// Loads all layers it can from a given folder
    pub fn from_folder(path: &Path) -> crate::Result<Self> {
        let folder = fs::read_dir(path)?;
        let mut files = folder
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let ty = LayerType::try_from(name.rsplit(".").next().unwrap());
                if matches!(entry.file_type(), Ok(ty) if ty.is_file()) && ty.is_ok() {
                    Some(
                        File::open(entry.path().as_path())
                            .map(|f| (name, f))
                            .map_err(|e| e.into()),
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

    /// Returns inner lasers
    pub fn layers(&self) -> Vec<&Layer> {
        self.0.iter().collect()
    }

    /// Returns inner lasers mutable
    pub fn layers_mut(&mut self) -> Vec<&mut Layer> {
        self.0.iter_mut().collect()
    }

    /// Merges layer if it already exists or adds a new one
    pub fn add_layer(&mut self, layer: Layer) {
        let existing = self.0.iter_mut().find(|e| e.ty == layer.ty);
        if let Some(existing) = existing {
            existing.data.merge(&layer.data)
        } else {
            self.0.push(layer);
        }
    }

    /// Returns a given layer by its type
    pub fn get_layer(&self, ty: &LayerType) -> Option<&Layer> {
        self.0.iter().find(|layer| &layer.ty == ty)
    }

    /// Returns a given layer by its type mutbale
    pub fn get_layer_mut(&mut self, ty: &LayerType) -> Option<&mut Layer> {
        self.0.iter_mut().find(|layer| &layer.ty == ty)
    }

    /// Writes layers to a given output
    ///
    /// @see Self::write_to_folder for an example
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

    /// Writes layers to a given folder
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
    /// Returns corners of board
    ///
    /// Will get them by getting min and max coords of each layer
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
    /// Adds an offset to all layers
    fn transform(&mut self, transform: &Pos) {
        for layer in &mut self.0 {
            layer.data.transform(transform);
        }
    }
}

impl LayerMerge for Board {
    /// Will merge all layers of same type else it will insert the layer
    fn merge(&mut self, other: &Self) {
        for layer in &mut self.0 {
            if let Some(other) = other.get_layer(&layer.ty) {
                layer.data.merge(&other.data)
            }
        }
    }
}
