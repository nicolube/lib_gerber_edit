use crate::error::ParseError;
use crate::gerber_types::{CoordinateOffset, InterpolationMode};
use crate::unit_able::UnitAble;
use crate::{
    LayerCorners, LayerData, LayerMerge, LayerScale, LayerStepAndRepeat, LayerTransform, LayerType,
    Pos,
};
use chrono::Utc;
use gerber_parser::GerberDoc;
use gerber_parser::gerber_types::{
    Aperture, ApertureDefinition, ApertureMacro, Command, CommentContent, CoordinateFormat,
    CoordinateMode, CoordinateNumber, Coordinates, DCode, ExtendedCode, FileAttribute,
    FunctionCode, GCode, GerberCode, GerberDate, GerberResult, ImageName, MCode, MacroContent,
    Operation, Polarity, QuadrantMode, StandardComment, StepAndRepeat, Unit, ZeroOmission,
};
use log::debug;
use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Read, Write};

/// A parsed RS-274X (Extended Gerber) layer, split into its logical components.
///
/// Constructed via [`GerberLayerData::from_type`], [`GerberLayerData::from_commands`],
/// or [`GerberLayerData::empty`]. Written back with [`GerberLayerData::write_to`].
///
/// The internal unit is always millimetres; [`GerberLayerData::to_unit`] performs
/// conversion if the source file uses inches.
#[derive(Debug, Clone, PartialEq)]
pub struct GerberLayerData {
    /// Logical role of this layer (copper top, silkscreen bottom, …).
    pub layer_type: LayerType,
    /// Integer/decimal digit counts used to encode coordinates in the file.
    pub coordinate_format: CoordinateFormat,
    /// Draw commands stripped of header/footer boilerplate.
    pub commands: Vec<Command>,
    /// Working unit (`Millimeters` after construction).
    unit: Unit,
    /// Aperture macros keyed by name.
    pub macros: HashMap<String, Vec<MacroContent>>,
    /// Aperture definitions keyed by D-code number.
    pub apertures: HashMap<i32, Aperture>,
    /// Non-fatal errors encountered during parsing, formatted as human-readable strings.
    pub parse_errors: Vec<String>,
}

impl GerberLayerData {
    /// Creates a new layer from already loaded gerber doc and its layer type
    pub fn new(ty: LayerType, data: GerberDoc) -> Result<Self, ParseError> {
        let units = data.units;
        let format_specification = data.format_specification;
        let apertures = data.apertures;

        let mut parse_errors = Vec::new();
        let mut all_commands = Vec::new();
        for result in data.commands {
            match result {
                Ok(cmd) => all_commands.push(cmd),
                Err(err) => parse_errors.push(err.to_string()),
            }
        }

        let macros = all_commands
            .iter()
            .filter_map(|cmd| match cmd {
                Command::ExtendedCode(ExtendedCode::ApertureMacro(mac)) => {
                    Some((mac.name.clone(), mac.content.clone()))
                }
                _ => None,
            })
            .collect::<HashMap<_, _>>();

        let commands: Vec<Command> = all_commands
            .into_iter()
            .skip_while(|cmd| {
                !matches!(
                    cmd,
                    Command::FunctionCode(
                        FunctionCode::DCode(_) | FunctionCode::GCode(GCode::RegionMode(_))
                    ) | Command::ExtendedCode(ExtendedCode::StepAndRepeat(_))
                )
            })
            .filter(|cmd| {
                !matches!(
                    cmd,
                    Command::FunctionCode(FunctionCode::MCode(MCode::EndOfFile))
                )
            })
            .collect();

        let ty = LayerType::from_commands(&commands).unwrap_or(ty);

        let coordinate_format =
            format_specification.ok_or(ParseError::FormatMissing(ty.to_string()))?;
        Ok(Self {
            unit: units.unwrap_or(Unit::Millimeters),
            coordinate_format,
            layer_type: ty,
            commands,
            macros,
            apertures,
            parse_errors,
        }
        .to_unit(&Unit::Millimeters))
    }

    /// Parses GerberLayer from reader with given type
    pub fn from_type<R>(ty: LayerType, reader: BufReader<R>) -> Result<Self, ParseError>
    where
        R: Read,
    {
        debug!("Parsing Gerber layer with type {:?}", ty);
        let data = gerber_parser::parse(reader).map_err(|(_, err)| err)?;
        Self::new(ty, data)
    }

    /// Parses GerberLayer from reader, type will load from FileAttribute
    pub fn from_commands<R>(reader: BufReader<R>) -> Result<Self, ParseError>
    where
        R: Read,
    {
        debug!("Parsing Gerber layer, detecting type from FileAttribute");
        let data = gerber_parser::parse(reader).map_err(|(_, err)| err)?;

        let layer_type =
            LayerType::from_commands(data.commands()).unwrap_or(LayerType::UndefinedGerber);

        Self::new(layer_type, data)
    }

    /// Creates an empty gerber layer
    pub fn empty(layer_type: LayerType) -> Self {
        Self {
            coordinate_format: CoordinateFormat::new(
                ZeroOmission::Leading,
                CoordinateMode::Absolute,
                4,
                6,
            ),
            apertures: HashMap::new(),
            layer_type,
            macros: HashMap::new(),
            unit: Unit::Millimeters,
            commands: Vec::new(),
            parse_errors: Vec::new(),
        }
    }

    /// Checks if layer is empty
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Converts all commands to given Unit
    pub fn to_unit(mut self, unit: &Unit) -> Self {
        if unit == &self.unit {
            return self;
        }

        self.coordinate_format =
            CoordinateFormat::new(ZeroOmission::Leading, CoordinateMode::Absolute, 4, 6);

        match unit {
            Unit::Millimeters => self.scale(25.4, 25.4),
            Unit::Inches => self.scale(1.0 / 25.4, 1.0 / 25.4),
        }

        for aperture in self.apertures.values_mut() {
            match aperture {
                Aperture::Circle(circle) => {
                    circle.diameter.convert_unit_self(&self.unit, unit);
                    circle.hole_diameter.convert_unit_self(&self.unit, unit);
                }
                Aperture::Obround(rect) | Aperture::Rectangle(rect) => {
                    rect.x.convert_unit_self(&self.unit, unit);
                    rect.y.convert_unit_self(&self.unit, unit);
                    rect.hole_diameter.convert_unit_self(&self.unit, unit);
                }
                Aperture::Polygon(poly) => {
                    poly.diameter.convert_unit_self(&self.unit, unit);
                    poly.hole_diameter.convert_unit_self(&self.unit, unit);
                }
                // TODO: whatever we need to do here.
                Aperture::Macro(_, Some(_)) => {}
                Aperture::Macro(_, None) => {}
            };
        }

        self.macros.values_mut().for_each(|contents| {
            contents.iter_mut().for_each(|content| {
                content.convert_unit_self(&self.unit, unit);
            });
        });

        self.unit = *unit;
        self
    }

    /// Assembles a complete, self-contained Gerber command stream.
    ///
    /// Prepends the standard header (file attributes, unit, coordinate format,
    /// aperture definitions, aperture macros) and appends `M02*` (end of file).
    /// The draw commands are passed through the [`Optimize`] filter before output.
    fn to_commands(&self) -> Vec<Command> {
        let mut commands = vec![Command::FunctionCode(FunctionCode::GCode(GCode::Comment(
            CommentContent::String("ProAdm generated panel".to_string()),
        )))];
        commands.extend(
            [
                FileAttribute::FileFunction(self.layer_type.function()),
                FileAttribute::CreationDate(GerberDate::from(Utc::now())),
            ]
            .into_iter()
            .map(|fa| {
                Command::FunctionCode(FunctionCode::GCode(GCode::Comment(
                    CommentContent::Standard(StandardComment::FileAttribute(fa)),
                )))
            }),
        );
        commands.extend([
            Command::FunctionCode(FunctionCode::GCode(GCode::QuadrantMode(
                QuadrantMode::Multi,
            ))),
            Command::ExtendedCode(ExtendedCode::Unit(self.unit)),
            Command::ExtendedCode(ExtendedCode::CoordinateFormat(self.coordinate_format)),
            Command::ExtendedCode(ExtendedCode::LoadPolarity(Polarity::Dark)),
            Command::ExtendedCode(ExtendedCode::ImageName(ImageName {
                name: self.layer_type.file_ending().to_string(),
            })),
        ]);

        commands.extend(
            self.apertures
                .iter()
                .filter(|(_, current)| !matches!(current, Aperture::Macro(..)))
                .map(|(id, aperture)| {
                    Command::ExtendedCode(ExtendedCode::ApertureDefinition(ApertureDefinition {
                        code: *id,
                        aperture: aperture.clone(),
                    }))
                }),
        );
        commands.extend(
            self.macros
                .iter()
                .map(|(name, content)| ApertureMacro {
                    name: name.clone(),
                    content: content.clone(),
                })
                .map(|x| Command::ExtendedCode(ExtendedCode::ApertureMacro(x.clone()))),
        );
        commands.extend(
            self.apertures
                .iter()
                .filter(|(_, current)| matches!(current, Aperture::Macro(..)))
                .map(|(id, aperture)| {
                    Command::ExtendedCode(ExtendedCode::ApertureDefinition(ApertureDefinition {
                        code: *id,
                        aperture: aperture.clone(),
                    }))
                }),
        );
        commands.extend(Command::optimize(&self.commands).into_iter().cloned());
        commands.push(Command::FunctionCode(FunctionCode::MCode(MCode::EndOfFile)));
        commands
    }

    /// Writes to given output
    pub fn write_to<T>(&self, writer: &mut BufWriter<T>) -> GerberResult<()>
    where
        T: Write,
    {
        self.to_commands().serialize(writer)?;
        writer.flush()?;
        Ok(())
    }
}

fn min_max(
    min: &mut Pos,
    max: &mut Pos,
    coords: &Coordinates,
    unit: &Unit,
    tool: &Option<(Pos, Pos)>,
) {
    let default = (Pos::default(), Pos::default());
    let (tool_min, tool_max) = tool.as_ref().unwrap_or(&default);

    if let Some(x) = coords.x {
        let x = f64::from(x.to_mm(unit));
        min.x = min.x.min(x + tool_min.x);
        max.x = max.x.max(x + tool_max.x);
    }

    if let Some(y) = coords.y {
        let y = f64::from(y.to_mm(unit));
        min.y = min.y.min(y + tool_min.y);
        max.y = max.y.max(y + tool_max.y);
    }
}

impl LayerTransform for GerberLayerData {
    fn transform(&mut self, pos: &Pos) {
        (&self.unit, &mut self.commands).transform(pos);
    }
}

impl LayerScale for GerberLayerData {
    fn scale(&mut self, x: f64, y: f64) {
        (&self.coordinate_format, &mut self.commands).scale(x, y);
    }
}

impl LayerMerge for GerberLayerData {
    fn merge(&mut self, other: &Self) {
        let mut aperture_id = 10;
        let mut aperture_id_map = HashMap::new();

        let other = other.clone().to_unit(&self.unit);

        for (id, aperture) in &other.apertures {
            let found_id =
                self.apertures.iter().find_map(
                    |(id, current)| {
                        if current == aperture { Some(id) } else { None }
                    },
                );
            while self.apertures.contains_key(&aperture_id) && found_id.is_none() {
                aperture_id += 1;
            }

            let aperture_id = *found_id.unwrap_or(&aperture_id);
            aperture_id_map.insert(id, aperture_id);
            self.apertures.insert(aperture_id, aperture.clone());
        }

        for command in other.commands {
            if let Command::FunctionCode(FunctionCode::DCode(DCode::SelectAperture(id))) = command {
                let d_code = aperture_id_map.get(&id).unwrap();
                self.commands
                    .push(Command::FunctionCode(FunctionCode::DCode(
                        DCode::SelectAperture(*d_code),
                    )));
            } else if let Command::FunctionCode(FunctionCode::DCode(DCode::Operation(op))) = command
            {
                let mut op = op.clone();
                match &mut op {
                    Operation::Interpolate(Some(cords), _)
                    | Operation::Move(Some(cords))
                    | Operation::Flash(Some(cords)) => {
                        cords.format = self.coordinate_format;
                    }
                    _ => {}
                };
                if let Operation::Interpolate(_, Some(cords)) = &mut op {
                    cords.format = self.coordinate_format
                }
                self.commands
                    .push(Command::FunctionCode(FunctionCode::DCode(
                        DCode::Operation(op),
                    )));
            } else {
                self.commands.push(command.clone());
            }
        }
    }
}

impl LayerStepAndRepeat for GerberLayerData {
    fn step_and_repeat(&mut self, x_repetitions: u32, y_repetitions: u32, offset: &Pos) {
        (&self.unit, &mut self.commands).step_and_repeat(x_repetitions, y_repetitions, offset);
    }
}

fn cmd_into_coords(command: &Command) -> Option<&Coordinates> {
    if let Command::FunctionCode(FunctionCode::DCode(DCode::Operation(
        Operation::Move(Some(coords))
        | Operation::Flash(Some(coords))
        | Operation::Interpolate(Some(coords), _),
    ))) = command
    {
        Some(coords)
    } else {
        None
    }
}

/// Updates a position in-place from modal coordinates (omitted axis keeps its current value).
fn update_pos(pos: &mut Pos, coords: &Coordinates, unit: &Unit) {
    if let Some(x) = coords.x {
        pos.x = f64::from(x.to_mm(unit));
    }
    if let Some(y) = coords.y {
        pos.y = f64::from(y.to_mm(unit));
    }
}

/// Returns the bounding box of a circular arc.
///
/// `offset` is the I/J vector from `start` to the arc center.
/// Cardinal axis-extremes of the circle (0°, 90°, 180°, 270°) are included
/// when they fall within the arc's sweep.
///
/// # Spec compliance (RS-274X §5.3)
/// This function correctly handles **multi-quadrant (G75)** arcs where I/J are
/// signed offsets. **Single-quadrant (G74)** arcs are not fully handled: in G74
/// mode I/J are unsigned magnitudes and the actual direction to the center is
/// determined implicitly from which of the four possible centers lies on the
/// perpendicular bisector of the chord. Since the parser delivers the raw
/// positive values, this function would only compute the correct center when
/// the center lies in the +X/+Y direction from start. In practice nearly all
/// modern Gerber files use G75, and the spec itself recommends always
/// specifying the quadrant mode explicitly.
fn arc_corners(
    start: &Pos,
    end: &Pos,
    offset: &CoordinateOffset,
    unit: &Unit,
    mode: InterpolationMode,
) -> (Pos, Pos) {
    use std::f64::consts::{FRAC_PI_2, PI};

    let i = offset.x.map(|v| f64::from(v.to_mm(unit))).unwrap_or(0.0);
    let j = offset.y.map(|v| f64::from(v.to_mm(unit))).unwrap_or(0.0);

    let cx = start.x + i;
    let cy = start.y + j;
    let radius = (i * i + j * j).sqrt();

    let start_angle = (start.y - cy).atan2(start.x - cx);
    let end_angle = (end.y - cy).atan2(end.x - cx);

    let mut min_x = start.x.min(end.x);
    let mut min_y = start.y.min(end.y);
    let mut max_x = start.x.max(end.x);
    let mut max_y = start.y.max(end.y);

    for (angle, px, py) in [
        (0.0_f64, cx + radius, cy),
        (FRAC_PI_2, cx, cy + radius),
        (PI, cx - radius, cy),
        (-FRAC_PI_2, cx, cy - radius),
    ] {
        if angle_in_arc(start_angle, end_angle, angle, mode) {
            min_x = min_x.min(px);
            min_y = min_y.min(py);
            max_x = max_x.max(px);
            max_y = max_y.max(py);
        }
    }

    (Pos { x: min_x, y: min_y }, Pos { x: max_x, y: max_y })
}

/// Returns true if `angle` (radians) is swept when travelling from `start` to
/// `end` in the given circular interpolation direction.
fn angle_in_arc(start: f64, end: f64, angle: f64, mode: InterpolationMode) -> bool {
    use std::f64::consts::PI;

    let norm = |a: f64| {
        let a = a.rem_euclid(2.0 * PI);
        if a > PI { a - 2.0 * PI } else { a }
    };

    // For CW, sweeping clockwise from start→end is the same as CCW from end→start.
    let (arc_start, arc_end) = match mode {
        InterpolationMode::CounterclockwiseCircular => (norm(start), norm(end)),
        InterpolationMode::ClockwiseCircular => (norm(end), norm(start)),
        InterpolationMode::Linear => return false,
    };

    let angle = norm(angle);

    // Equal start/end means a full circle.
    if (arc_start - arc_end).abs() < 1e-10 {
        return true;
    }

    if arc_start <= arc_end {
        angle >= arc_start && angle <= arc_end
    } else {
        // Arc crosses the ±π discontinuity.
        angle >= arc_start || angle <= arc_end
    }
}

impl LayerCorners for (&Unit, &Vec<Command>) {
    fn get_corners(&self) -> (Pos, Pos) {
        let (unit, cmds) = self;
        let mut min = Pos {
            x: f64::MAX,
            y: f64::MAX,
        };
        let mut max = Pos {
            x: f64::MIN,
            y: f64::MIN,
        };
        for command in cmds.iter() {
            if let Some(coords) = cmd_into_coords(command) {
                min_max(&mut min, &mut max, coords, unit, &None);
            }
        }
        (min, max)
    }
}

impl LayerCorners for GerberLayerData {
    fn get_corners(&self) -> (Pos, Pos) {
        let unit = &self.unit;
        let mut min = Pos {
            x: f64::MAX,
            y: f64::MAX,
        };
        let mut max = Pos {
            x: f64::MIN,
            y: f64::MIN,
        };
        let mut tool = None;
        let mut current_pos = Pos::default();
        let mut interp_mode = InterpolationMode::Linear;

        for command in self.commands.iter() {
            match command {
                Command::FunctionCode(FunctionCode::DCode(DCode::SelectAperture(ap))) => {
                    if let Some(ap) = self.apertures.get(ap) {
                        tool = Some(ap.get_corners());
                    }
                }
                Command::FunctionCode(FunctionCode::GCode(GCode::InterpolationMode(mode))) => {
                    interp_mode = *mode;
                }
                Command::FunctionCode(FunctionCode::DCode(DCode::Operation(op))) => match op {
                    Operation::Move(Some(coords)) => {
                        update_pos(&mut current_pos, coords, unit);
                        // Include move targets so stroke starts are bounded (no tool: not drawing).
                        min_max(&mut min, &mut max, coords, unit, &None);
                    }
                    Operation::Flash(Some(coords)) => {
                        update_pos(&mut current_pos, coords, unit);
                        min_max(&mut min, &mut max, coords, unit, &tool);
                    }
                    Operation::Interpolate(Some(end_coords), Some(offset))
                        if interp_mode != InterpolationMode::Linear =>
                    {
                        let start = current_pos.clone();
                        update_pos(&mut current_pos, end_coords, unit);
                        let (arc_min, arc_max) =
                            arc_corners(&start, &current_pos, offset, unit, interp_mode);
                        let default = (Pos::default(), Pos::default());
                        let (tool_min, tool_max) = tool.as_ref().unwrap_or(&default);
                        min.x = min.x.min(arc_min.x + tool_min.x);
                        min.y = min.y.min(arc_min.y + tool_min.y);
                        max.x = max.x.max(arc_max.x + tool_max.x);
                        max.y = max.y.max(arc_max.y + tool_max.y);
                    }
                    Operation::Interpolate(Some(coords), _) => {
                        update_pos(&mut current_pos, coords, unit);
                        min_max(&mut min, &mut max, coords, unit, &tool);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        (min, max)
    }
}

impl LayerCorners for Aperture {
    fn get_corners(&self) -> (Pos, Pos) {
        // Minimum aperture size (mm) to include in bounding box calculations.
        // Thin traces/lines are excluded so they don't inflate board dimensions.
        const MIN_TOOL_SIZE: f64 = 0.1;
        let (x_size, y_size) = match self {
            Aperture::Circle(c) => (c.diameter, c.diameter),
            Aperture::Obround(r) | Aperture::Rectangle(r) => (r.x, r.y),
            Aperture::Polygon(_) | Aperture::Macro(_, _) => (0.0, 0.0),
        };
        if x_size < MIN_TOOL_SIZE && y_size < MIN_TOOL_SIZE {
            return (Pos::default(), Pos::default());
        }
        let (x, y) = (x_size / 2.0, y_size / 2.0);
        (Pos { x: -x, y: -y }, Pos { x, y })
    }
}

impl LayerTransform for (&Unit, &mut Vec<Command>) {
    fn transform(&mut self, transform: &Pos) {
        let (unit, cmds) = self;
        let to_unit = |x: f64| CoordinateNumber::try_from(x.mm_to_unit(unit)).ok();
        let to_mm = |x: &CoordinateNumber| f64::from(*x).to_mm(unit);
        for command in cmds.iter_mut() {
            if let Command::FunctionCode(FunctionCode::DCode(DCode::Operation(
                Operation::Move(Some(coords))
                | Operation::Flash(Some(coords))
                | Operation::Interpolate(Some(coords), _),
            ))) = command
            {
                if let Some(x) = coords.x {
                    coords.x = to_unit(to_mm(&x) + transform.x);
                }
                if let Some(y) = coords.y {
                    coords.y = to_unit(to_mm(&y) + transform.y);
                }
            }
        }
    }
}

impl LayerScale for (&CoordinateFormat, &mut Vec<Command>) {
    fn scale(&mut self, x: f64, y: f64) {
        let (format, cmds) = self;
        for cmd in cmds.iter_mut() {
            if let Command::FunctionCode(FunctionCode::DCode(DCode::Operation(op))) = cmd {
                match op {
                    Operation::Interpolate(Some(pos), _)
                    | Operation::Move(Some(pos))
                    | Operation::Flash(Some(pos)) => {
                        pos.scale(x, y);
                        pos.format = **format;
                    }
                    _ => {}
                }
                if let Operation::Interpolate(_, Some(pos)) = op {
                    pos.scale(x, y);
                    pos.format = **format;
                }
            }
        }
    }
}

macro_rules! coord_scaling {
    ($name:ident) => {
        impl LayerScale for $name {
            fn scale(&mut self, x: f64, y: f64) {
                self.x = self.x.map(|v| {
                    CoordinateNumber::try_from(f64::from(v) * x)
                        .expect("coordinate overflow: scale factor too large")
                });
                self.y = self.y.map(|v| {
                    CoordinateNumber::try_from(f64::from(v) * y)
                        .expect("coordinate overflow: scale factor too large")
                });
            }
        }
    };
}

coord_scaling!(Coordinates);
coord_scaling!(CoordinateOffset);

impl LayerStepAndRepeat for (&Unit, &mut Vec<Command>) {
    fn step_and_repeat(&mut self, x_repetitions: u32, y_repetitions: u32, offset: &Pos) {
        let sr = Command::ExtendedCode(ExtendedCode::StepAndRepeat(StepAndRepeat::Open {
            repeat_x: x_repetitions,
            repeat_y: y_repetitions,
            distance_x: offset.x.mm_to_unit(self.0),
            distance_y: offset.y.mm_to_unit(self.0),
        }));

        let commands = &mut self.1;
        commands.insert(0, sr);
        // We do this because Avion is stupid and does not update their software...
        // Well for some reason they do not interpret SR close correctly so we put a 1x1 SR here
        // witch results in the same behavior then SR close.
        const SR_RESET_DUMMY: Command =
            Command::ExtendedCode(ExtendedCode::StepAndRepeat(StepAndRepeat::Open {
                repeat_x: 1,
                repeat_y: 1,
                distance_x: 0f64,
                distance_y: 0f64,
            }));
        commands.push(SR_RESET_DUMMY);
        commands.push(Command::ExtendedCode(ExtendedCode::StepAndRepeat(
            StepAndRepeat::Close,
        )))
    }
}

impl From<GerberLayerData> for LayerData {
    fn from(value: GerberLayerData) -> Self {
        LayerData::Gerber(value)
    }
}

/// Deduplicates modal Gerber state commands.
///
/// In RS-274X several codes are *modal* — once set, they remain active until
/// explicitly changed. Emitting the same `SelectAperture`, `InterpolationMode`,
/// or `QuadrantMode` command twice in a row is legal but redundant.
/// `Optimize::optimize` strips the duplicates, shrinking the output file.
pub trait Optimize {
    /// Returns a filtered slice that omits consecutive repeated modal commands.
    fn optimize(values: &[Self]) -> Vec<&Self>
    where
        Self: Sized;
}

#[derive(Debug, Default)]
struct OptimizationContext<'a> {
    quadrant_mode: Option<&'a QuadrantMode>,
    aperture: Option<&'a i32>,
    interpolation_mode: Option<&'a InterpolationMode>,
}

impl Optimize for Command {
    fn optimize(values: &[Self]) -> Vec<&Self> {
        let mut result = Vec::new();
        let mut context = OptimizationContext::default();
        for command in values.iter() {
            match command {
                Command::ExtendedCode(ExtendedCode::StepAndRepeat(_)) => {
                    context = OptimizationContext::default();
                }
                Command::FunctionCode(FunctionCode::DCode(DCode::SelectAperture(current))) => {
                    if Some(current) == context.aperture {
                        continue;
                    }
                    context.aperture = Some(current)
                }
                Command::FunctionCode(FunctionCode::GCode(GCode::InterpolationMode(current))) => {
                    if Some(current) == context.interpolation_mode {
                        continue;
                    }
                    context.interpolation_mode = Some(current)
                }
                Command::FunctionCode(FunctionCode::GCode(GCode::QuadrantMode(current))) => {
                    if Some(current) == context.quadrant_mode {
                        continue;
                    }
                    context.quadrant_mode = Some(current)
                }
                _ => {}
            }
            result.push(command);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Size;
    use std::fs::File;

    #[test]
    fn test_layer_size() -> Result<(), Box<dyn std::error::Error>> {
        let layer = GerberLayerData::from_type(
            LayerType::Dimensions,
            BufReader::new(File::open("test/test_outline.gbr")?),
        )?;
        assert_eq!(
            layer.get_size(),
            Size {
                width: 112.0,
                height: 132.0
            }
        );
        Ok(())
    }

    // ── Arc bounding-box tests ────────────────────────────────────────────────
    //
    // All tests use multi-quadrant mode (G75) as required by RS-274X §5.3.3.
    // Coordinates are in FSLAX46Y46 format (1 unit = 10⁻⁶ mm), so
    // 1 mm = 1_000_000 units.  Aperture diameter 0.1 mm → half-width 0.05 mm.

    /// RS-274X §5.3.4 worked example: CCW arc from (3,−2) to (−3,−2).
    /// Center offset I=−3 J=4 → center (0, 2), radius 5.
    /// Arc sweeps ~286° CCW through the top; the bottommost cardinal (0,−3)
    /// is **not** swept and must not inflate y_min.
    ///
    /// Verified manually:
    ///   start_angle  = atan2(−4, 3)  ≈ −53°
    ///   end_angle    = atan2(−4, −3) ≈ −127°  (normalised ≈ −2.21 rad)
    ///   CCW: sweeps from −53° increasing → passes 0°, 90°, 180° before
    ///        reaching −127°, so right (5,2), top (0,7), left (−5,2) included.
    ///   −90° (bottom, (0,−3)) is NOT in [−53°, −127°] CCW → excluded. ✓
    ///
    /// Expected bbox (tool half = 0.05 mm):
    ///   x ∈ [−5.05, 5.05],  y ∈ [−2.05, 7.05]
    #[test]
    fn test_arc_ccw_through_top() -> Result<(), Box<dyn std::error::Error>> {
        let gbr = "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,0.100000*%
D10*
G75*
G01*
X3000000Y-2000000D02*
G03*
X-3000000Y-2000000I-3000000J4000000D01*
M02*
";
        let layer = GerberLayerData::from_type(LayerType::Top, BufReader::new(gbr.as_bytes()))?;
        let (min, max) = layer.get_corners();
        let eps = 1e-6;
        assert!(
            (min.x - (-5.05)).abs() < eps,
            "min.x: expected -5.05, got {}",
            min.x
        );
        assert!(
            (max.x - 5.05).abs() < eps,
            "max.x: expected  5.05, got {}",
            max.x
        );
        assert!(
            (min.y - (-2.05)).abs() < eps,
            "min.y: expected -2.05, got {}",
            min.y
        );
        assert!(
            (max.y - 7.05).abs() < eps,
            "max.y: expected  7.05, got {}",
            max.y
        );
        Ok(())
    }

    /// CW quarter-circle from (5, 0) to (0, −5), center (0, 0), radius 5.
    /// Center offset I=−5 J=0.
    /// Arc sweeps 90° CW through the fourth quadrant only.
    ///
    /// Cardinals at 0° (5, 0) and −90° (0, −5) are on the arc boundary.
    /// Cardinals at 90° and 180° are NOT swept → must not inflate the bbox.
    ///
    /// Expected bbox (tool half = 0.05 mm):
    ///   x ∈ [−0.05, 5.05],  y ∈ [−5.05, 0.05]
    #[test]
    fn test_arc_cw_quarter() -> Result<(), Box<dyn std::error::Error>> {
        let gbr = "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,0.100000*%
D10*
G75*
G01*
X5000000Y0D02*
G02*
X0Y-5000000I-5000000J0D01*
M02*
";
        let layer = GerberLayerData::from_type(LayerType::Top, BufReader::new(gbr.as_bytes()))?;
        let (min, max) = layer.get_corners();
        let eps = 1e-6;
        assert!(
            (min.x - (-0.05)).abs() < eps,
            "min.x: expected -0.05, got {}",
            min.x
        );
        assert!(
            (max.x - 5.05).abs() < eps,
            "max.x: expected  5.05, got {}",
            max.x
        );
        assert!(
            (min.y - (-5.05)).abs() < eps,
            "min.y: expected -5.05, got {}",
            min.y
        );
        assert!(
            (max.y - 0.05).abs() < eps,
            "max.y: expected  0.05, got {}",
            max.y
        );
        Ok(())
    }

    /// Full circle: start == end in multi-quadrant mode means 360° (§5.3.3).
    /// Circle centred at (0, 0), radius 5: start and end both at (5, 0),
    /// center offset I=−5 J=0.
    /// All four cardinals must be included.
    ///
    /// Expected bbox (tool half = 0.05 mm):
    ///   x ∈ [−5.05, 5.05],  y ∈ [−5.05, 5.05]
    #[test]
    fn test_arc_full_circle() -> Result<(), Box<dyn std::error::Error>> {
        let gbr = "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,0.100000*%
D10*
G75*
G01*
X5000000Y0D02*
G03*
X5000000Y0I-5000000J0D01*
M02*
";
        let layer = GerberLayerData::from_type(LayerType::Top, BufReader::new(gbr.as_bytes()))?;
        let (min, max) = layer.get_corners();
        let eps = 1e-6;
        assert!(
            (min.x - (-5.05)).abs() < eps,
            "min.x: expected -5.05, got {}",
            min.x
        );
        assert!(
            (max.x - 5.05).abs() < eps,
            "max.x: expected  5.05, got {}",
            max.x
        );
        assert!(
            (min.y - (-5.05)).abs() < eps,
            "min.y: expected -5.05, got {}",
            min.y
        );
        assert!(
            (max.y - 5.05).abs() < eps,
            "max.y: expected  5.05, got {}",
            max.y
        );
        Ok(())
    }
}
