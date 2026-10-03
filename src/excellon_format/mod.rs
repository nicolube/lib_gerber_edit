use crate::error::MergeError;
use crate::unit_able::UnitAble;
use crate::{
    LayerCorners, LayerData, LayerMerge, LayerRotate, LayerScale, LayerStepAndRepeat,
    LayerTransform, Pos,
};
use derive_more::{Display, Error};
use gerber_parser::gerber_types::Unit;
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::num::{ParseFloatError, ParseIntError};
use winnow::ModalResult;
use winnow::Parser;
use winnow::ascii::{digit1, float};
use winnow::combinator::{alt, opt, preceded, repeat};
use winnow::error::ContextError;
use winnow::token::{one_of, rest, take};

/// A parsed Excellon drill file, split into header and body sections.
///
/// Parse errors on individual lines are stored inline as `Err` variants rather
/// than aborting the whole parse, so callers can decide how to handle them.
#[derive(Debug, Clone, PartialEq)]
pub struct ExcellonLayerData {
    /// Header commands (everything before `M95`/`%`), including unit and format definitions.
    pub header: Vec<Result<Command, ExcellonParseFormat>>,
    /// Body commands (drill hits, tool changes, end-of-program, …).
    pub commands: Vec<Result<Command, ExcellonParseFormat>>,
    /// Coordinate format derived from the header (unit, zero-suppression, digit counts).
    pub unit: UnitDefinition,
    /// Tool definitions: tool number → drill diameter in the file's native unit.
    pub tools: HashMap<u32, f64>,
}

impl ExcellonLayerData {
    /// Serialises the layer back to Excellon text format.
    ///
    /// Tool definitions are always written in sorted order immediately before
    /// the header-end marker, regardless of their original position.
    pub fn write_to<T>(&self, writer: &mut BufWriter<T>) -> std::io::Result<()>
    where
        T: Write,
    {
        let mut header_end = Command::Machine(MachineCode::HeaderEnd);
        for command in &self.header {
            match &command {
                Ok(Command::ToolDefinition(_)) => continue,
                Ok(Command::Machine(MachineCode::RewindStop))
                | Ok(Command::Machine(MachineCode::HeaderEnd)) => {
                    header_end = command.as_ref().unwrap().clone();
                    continue;
                }
                Ok(command) => {
                    write!(writer, "{}", command)?;
                }
                Err(_) => {}
            }
        }

        for (id, diameter) in &self.tools {
            let td = Command::ToolDefinition(ToolDefinition {
                diameter: *diameter,
                tool_number: *id,
            });
            write!(writer, "{}", td)?;
        }

        write!(writer, "{}", header_end)?;
        for command in self.commands.iter().flatten() {
            writer.write_all(command.to_string().as_bytes())?;
        }
        Ok(())
    }
    /// Returns `true` if the layer contains no drill hit coordinates.
    pub fn is_empty(&self) -> bool {
        !self.commands.iter().any(|x| {
            matches!(
                x,
                Ok(Command::Coordinate(..)) | Ok(Command::Slot { .. }) | Ok(Command::Arc { .. })
            )
        })
    }
}

impl LayerTransform for ExcellonLayerData {
    fn transform(&mut self, transform: &Pos) {
        let shift = |v: &mut Option<f64>, delta: f64, unit: &Unit| {
            *v = v.to_mm(unit).map(|n| n + delta).mm_to_unit(unit);
        };
        for cmd in self
            .header
            .iter_mut()
            .chain(self.commands.iter_mut())
            .filter_map(|x| x.as_mut().ok())
        {
            match cmd {
                Command::Coordinate(x, y, fmt) => {
                    shift(x, transform.x, &fmt.unit);
                    shift(y, transform.y, &fmt.unit);
                }
                Command::Slot {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                    fmt,
                } => {
                    shift(from_x, transform.x, &fmt.unit);
                    shift(to_x, transform.x, &fmt.unit);
                    shift(from_y, transform.y, &fmt.unit);
                    shift(to_y, transform.y, &fmt.unit);
                }
                // Centre offsets and radii are relative, so only the end moves.
                Command::Arc { x, y, fmt, .. } => {
                    shift(x, transform.x, &fmt.unit);
                    shift(y, transform.y, &fmt.unit);
                }
                _ => {}
            }
        }
    }
}

/// Rewrites a drill program to absolute coordinates with both axes written.
///
/// Incremental programs (`ICI` header, `G91`) are resolved and switched to
/// absolute, and omitted (modal) axes are filled from the current point.
/// Coordinates at `repeat_steps` (expanded `R` codes) are deltas in any mode.
/// After this, transforms, merges and bounds never depend on the input mode or
/// on earlier commands.
fn resolve_absolute_coordinates(
    commands: &mut [Result<Command, ExcellonParseFormat>],
    repeat_steps: &HashSet<usize>,
) {
    let mut incremental = false;
    // Current point in mm, so unit switches (M71/M72) between commands stay exact.
    let mut current = (0.0, 0.0);
    let mut resolve = |x: &mut Option<f64>, y: &mut Option<f64>, unit: &Unit, incremental: bool| {
        let axis = |v: &Option<f64>, cur: f64| match v {
            Some(v) if incremental => cur + v.to_mm(unit),
            Some(v) => v.to_mm(unit),
            None => cur,
        };
        current = (axis(x, current.0), axis(y, current.1));
        *x = Some(current.0.mm_to_unit(unit));
        *y = Some(current.1.mm_to_unit(unit));
    };
    for (index, cmd) in commands.iter_mut().enumerate() {
        let Ok(cmd) = cmd else { continue };
        match cmd {
            Command::Incremental(i) => {
                incremental = *i;
                *i = false;
            }
            Command::Geometric(GeometricCode::InputMode(m)) => {
                incremental = *m == InputMode::Incremental;
                *m = InputMode::Absolute;
            }
            Command::Coordinate(x, y, fmt) => resolve(
                x,
                y,
                &fmt.unit,
                incremental || repeat_steps.contains(&index),
            ),
            Command::Slot {
                from_x,
                from_y,
                to_x,
                to_y,
                fmt,
            } => {
                resolve(from_x, from_y, &fmt.unit, incremental);
                resolve(to_x, to_y, &fmt.unit, incremental);
            }
            Command::Arc { x, y, fmt, .. } => resolve(x, y, &fmt.unit, incremental),
            _ => {}
        }
    }
}

/// Rotates every drill coordinate by `rot` CW quarter-turns about the origin.
///
/// Rotation mixes both axes, so both are always written; an omitted axis is
/// filled from the current point (coordinates are absolute after load).
fn rotate_excellon_coordinates(e: &mut ExcellonLayerData, rot: i32) {
    let mut current = (0.0, 0.0);
    let mut rot_point = |x: &mut Option<f64>, y: &mut Option<f64>| {
        current = (x.unwrap_or(current.0), y.unwrap_or(current.1));
        let (rx, ry) = crate::rotate_90(current.0, current.1, rot);
        *x = Some(rx);
        *y = Some(ry);
    };
    for cmd in e
        .header
        .iter_mut()
        .chain(e.commands.iter_mut())
        .filter_map(|c| c.as_mut().ok())
    {
        match cmd {
            Command::Coordinate(x, y, _) => rot_point(x, y),
            Command::Slot {
                from_x,
                from_y,
                to_x,
                to_y,
                ..
            } => {
                rot_point(from_x, from_y);
                rot_point(to_x, to_y);
            }
            Command::Arc { x, y, center, .. } => {
                rot_point(x, y);
                // A quarter turn keeps the arc direction; only the centre
                // offset vector turns with the points.
                if let ArcCenter::Offset { i, j } = center {
                    (*i, *j) = crate::rotate_90(*i, *j, rot);
                }
            }
            _ => {}
        }
    }
}

impl LayerRotate for ExcellonLayerData {
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
        let steps = steps.rem_euclid(4);
        if steps != 0 {
            rotate_excellon_coordinates(self, steps);
        }
        self.transform(offset);
    }
}

impl LayerMerge for ExcellonLayerData {
    /// Excellon programs carry no state that can make a merge fail.
    fn merge(&mut self, other: &Self) -> Result<(), MergeError> {
        self.append(other);
        Ok(())
    }
}

impl ExcellonLayerData {
    /// Appends `other`, remapping tool numbers.
    fn append(&mut self, other: &Self) {
        let mut next_free = 1;
        let mut tool_map = HashMap::new();
        for tool in &other.tools {
            let dir = tool.1.to_unit(&other.unit.unit, &self.unit.unit);
            let id = self
                .tools
                .iter()
                .find_map(|(id, dia)| if dia == &dir { Some(*id) } else { None })
                .unwrap_or_else(|| {
                    while self.tools.contains_key(&next_free) {
                        next_free += 1;
                    }
                    next_free
                });
            tool_map.insert(tool.0, id);
            self.tools.insert(id, dir);
        }

        let mut last_unit = self
            .commands
            .iter()
            .rev()
            .find_map(|x| match x {
                Ok(Command::Machine(MachineCode::Scale(ec))) => Some(*ec),
                _ => None,
            })
            .unwrap_or(self.unit.unit);
        let mut last_tool = self.commands.iter().rev().find_map(|x| match x {
            Ok(Command::Tool(id)) => Some(*id),
            _ => None,
        });
        let mut last_mode = self.commands.iter().rev().find_map(|x| match x {
            Ok(Command::Geometric(GeometricCode::Mode(m))) => Some(*m),
            _ => None,
        });
        let mut last_input_mode = self
            .commands
            .iter()
            .rev()
            .find_map(|x| match x {
                Ok(Command::Geometric(GeometricCode::InputMode(im))) => Some(im.clone()),
                _ => None,
            })
            .or_else(|| {
                self.header.iter().rev().find_map(|x| match x {
                    Ok(Command::Incremental(false)) => Some(InputMode::Absolute),
                    Ok(Command::Incremental(true)) => Some(InputMode::Incremental),
                    _ => None,
                })
            })
            .unwrap_or(InputMode::Absolute);

        // Remove end of program code
        self.commands
            .retain(|x| !matches!(x, Ok(Command::Machine(MachineCode::EndOfProgram))));

        for command in other.commands.iter() {
            let mut command = command.clone();
            match &mut command {
                Ok(Command::Coordinate(_, _, fmt)) => {
                    fmt.leading = self.unit.leading;
                    fmt.trailing = self.unit.trailing;
                }
                Ok(Command::Slot { fmt, .. }) | Ok(Command::Arc { fmt, .. }) => {
                    fmt.leading = self.unit.leading;
                    fmt.trailing = self.unit.trailing;
                }
                Ok(Command::Tool(t)) => {
                    *t = *tool_map.get(t).unwrap();
                    if Some(*t) != last_tool {
                        last_tool = Some(*t);
                    } else {
                        continue;
                    }
                }
                Ok(Command::Geometric(GeometricCode::InputMode(im))) => {
                    if im != &last_input_mode {
                        last_input_mode = im.clone();
                    } else {
                        continue;
                    }
                }
                Ok(Command::Geometric(GeometricCode::Mode(m))) => {
                    if Some(&*m) != last_mode.as_ref() {
                        last_mode = Some(*m);
                    } else {
                        continue;
                    }
                }
                Ok(Command::Machine(MachineCode::Scale(sc))) => {
                    if sc != &last_unit {
                        last_unit = *sc;
                    } else {
                        continue;
                    }
                }
                _ => {}
            }
            self.commands.push(command);
        }
    }
}

impl LayerStepAndRepeat for ExcellonLayerData {
    fn step_and_repeat(&mut self, x_repetitions: u32, y_repetitions: u32, offset: &Pos) {
        let copy = self.clone();
        for y in 0..y_repetitions {
            for x in 0..x_repetitions {
                if x == 0 && y == 0 {
                    continue;
                }
                let pos = Pos {
                    x: x as f64 * offset.x,
                    y: y as f64 * offset.y,
                };
                let mut copy = copy.clone();
                copy.transform(&pos);
                self.append(&copy);
            }
        }
    }
}

impl LayerScale for ExcellonLayerData {
    fn scale(&mut self, x: f64, y: f64) {
        let mul = |v: &mut Option<f64>, factor: f64| {
            if let Some(v) = v {
                *v *= factor;
            }
        };
        for command in self.commands.iter_mut().filter_map(|c| c.as_mut().ok()) {
            match command {
                Command::Coordinate(cx, cy, _) => {
                    mul(cx, x);
                    mul(cy, y);
                }
                Command::Slot {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                    ..
                } => {
                    mul(from_x, x);
                    mul(to_x, x);
                    mul(from_y, y);
                    mul(to_y, y);
                }
                Command::Arc {
                    x: ax,
                    y: ay,
                    center,
                    ..
                } => {
                    mul(ax, x);
                    mul(ay, y);
                    // Exact only for uniform scaling; an ellipse cannot be
                    // expressed as a circular arc.
                    match center {
                        ArcCenter::Radius(r) => *r *= x.abs().max(y.abs()),
                        ArcCenter::Offset { i, j } => {
                            *i *= x;
                            *j *= y;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

impl LayerCorners for ExcellonLayerData {
    fn get_corners(&self) -> (Pos, Pos) {
        let mut min = Pos {
            x: f64::MAX,
            y: f64::MAX,
        };
        let mut max = Pos {
            x: f64::MIN,
            y: f64::MIN,
        };
        // Drill diameter (mm) of the currently selected tool.
        let mut radius = 0.0;
        // Coordinates are absolute after load (see `resolve_absolute_coordinates`).
        let mut current = Pos::default();
        let mut mode = Mode::DrillMode;
        // Widens the box by `lo..hi` padded with the tool radius.
        fn grow(min: &mut Pos, max: &mut Pos, lo: [f64; 2], hi: [f64; 2], pad: f64) {
            min.x = min.x.min(lo[0] - pad);
            min.y = min.y.min(lo[1] - pad);
            max.x = max.x.max(hi[0] + pad);
            max.y = max.y.max(hi[1] + pad);
        }

        for command in self.commands.iter().flatten() {
            match command {
                Command::Tool(id) => {
                    radius = self
                        .tools
                        .get(id)
                        .map(|d| d.to_mm(&self.unit.unit) / 2.0)
                        .unwrap_or(0.0);
                }
                Command::Geometric(GeometricCode::Mode(m)) => mode = *m,
                Command::Arc { x, y, center, fmt }
                    if matches!(mode, Mode::CircularCW | Mode::CircularCWW) =>
                {
                    let start = [current.x, current.y];
                    current = resolve_point(*x, *y, &fmt.unit, &current);
                    let end = [current.x, current.y];
                    let ccw = mode == Mode::CircularCWW;
                    let c = center.center_mm(start, end, &fmt.unit, ccw);
                    let (lo, hi) = crate::flatten::geom::arc_bounds(start, end, c, ccw);
                    grow(&mut min, &mut max, lo, hi, radius);
                }
                Command::Coordinate(x, y, fmt) | Command::Arc { x, y, fmt, .. } => {
                    current = resolve_point(*x, *y, &fmt.unit, &current);
                    let p = [current.x, current.y];
                    grow(&mut min, &mut max, p, p, radius);
                }
                Command::Slot {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                    fmt,
                } => {
                    // Both endpoints bound the milled slot; tool width expands it.
                    let from = resolve_point(*from_x, *from_y, &fmt.unit, &current);
                    current = resolve_point(*to_x, *to_y, &fmt.unit, &from);
                    for p in [[from.x, from.y], [current.x, current.y]] {
                        grow(&mut min, &mut max, p, p, radius);
                    }
                }
                _ => {}
            }
        }
        (min, max)
    }
}

/// Structured error type for individual Excellon parse failures.
#[derive(Debug, Clone, PartialEq, Error, Display)]
#[non_exhaustive]
pub enum ExcellonError {
    #[display("Invalid CIC format: {}", _0)]
    InvalidCicOption(#[error(not(source))] String),
    #[display("Invalid command format: {}", _0)]
    InvalidCmd(#[error(not(source))] String),
    #[display("Invalid tool definition: {}", _0)]
    InvalidToolDefinition(#[error(not(source))] String),
    #[display("Invalid coordinate format: {}", _0)]
    InvalidCoordinate(#[error(not(source))] String),
    #[display("Invalid unit definition")]
    InvalidUnitDefinition,
    #[display("Invalid geometric code: {}", _0)]
    InvalidGeometricCode(#[error(not(source))] u8),
    #[display("Invalid machine code: {}", _0)]
    InvalidMachineCode(#[error(not(source))] u8),
    /// Referenced tool number has no matching `T<n>C<diam>` definition.
    #[display("Invalid tool number: {}", _0)]
    InvalidToolNumber(#[error(not(source))] u32),
    #[display("Missing header-end marker (M95/%)")]
    MissingHeaderEnd,
    #[display("Missing header-start marker (M48)")]
    MissingHeaderStart,
    #[display("Missing end-of-program marker (M30)")]
    MissingEndOfProgram,
    #[display("Coordinate used before unit declaration")]
    CoordinateBeforeUnit,
    #[display("Failed to parse floating number: {}", _1)]
    FloatParse(#[error(source)] ParseFloatError, String),
    #[display("Failed to parse number: {}", _1)]
    IntParse(#[error(source)] ParseIntError, String),
    #[display("Failed to parse version number")]
    InvalidVersion,
    #[display("{}", _0)]
    Custom(#[error(not(source))] String),
}

impl From<ExcellonError> for std::io::Error {
    fn from(e: ExcellonError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    }
}

/// A coordinate in mm; an omitted axis keeps its value from `current`.
pub(crate) fn resolve_point(x: Option<f64>, y: Option<f64>, unit: &Unit, current: &Pos) -> Pos {
    Pos {
        x: x.map_or(current.x, |v| v.to_mm(unit)),
        y: y.map_or(current.y, |v| v.to_mm(unit)),
    }
}

/// A parse error annotated with its source line number and raw text.
#[derive(Debug, Clone, Error, PartialEq, Display)]
#[display("Excellon parse error at line {}: {}", line, content)]
pub struct ExcellonParseFormat {
    #[error(source)]
    source: ExcellonError,
    line: usize,
    content: String,
}

/// A single parsed Excellon command.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Command {
    /// `FMAT,<version>` — file format version declaration.
    FormatCode(u8),
    /// `ICI,ON` / `ICI,OFF` — incremental (`true`) or absolute (`false`) input.
    Incremental(bool),
    /// `METRIC` / `INCH` — unit, zero-suppression and digit-count declaration.
    UnitDefinition(UnitDefinition),
    /// G-code (mode, dwell, input mode).
    Geometric(GeometricCode),
    /// M-code (header delimiters, scale, end-of-program).
    Machine(MachineCode),
    /// `X<n>Y<n>` — a drill-hit coordinate pair plus its format context.
    Coordinate(Option<f64>, Option<f64>, UnitDefinition),
    /// `T<n>` — select tool by number.
    Tool(u32),
    /// `T<n>C<diam>` — define a tool (number + diameter).
    ToolDefinition(ToolDefinition),
    /// `;…` — comment line.
    Comment(String),
    /// `F<n>` — feed-rate setting.
    FeedRate(u32),
    /// `[X<a>Y<b>]G85X<c>Y<d>` — route (mill) a slot between two points.
    Slot {
        from_x: Option<f64>,
        from_y: Option<f64>,
        to_x: Option<f64>,
        to_y: Option<f64>,
        fmt: UnitDefinition,
    },
    /// `X<n>Y<n>A<r>` / `X<n>Y<n>I<n>J<n>` — a circular route to `(x, y)`;
    /// the direction comes from the active `G02`/`G03` mode.
    Arc {
        x: Option<f64>,
        y: Option<f64>,
        center: ArcCenter,
        fmt: UnitDefinition,
    },
}

/// How a circular route gives its centre, in the coordinate's unit.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum ArcCenter {
    /// `A<r>`: the arc of radius `r` up to 180°; a negative radius selects
    /// the arc over 180°.
    Radius(f64),
    /// `I<i>J<j>`: centre offset from the arc start.
    Offset { i: f64, j: f64 },
}

impl ArcCenter {
    /// Centre in mm of the arc from `start` to `end` (both mm); `unit` is the
    /// unit the radius or offsets are written in.
    pub fn center_mm(&self, start: [f64; 2], end: [f64; 2], unit: &Unit, ccw: bool) -> [f64; 2] {
        match *self {
            ArcCenter::Radius(r) => {
                crate::flatten::geom::arc_center_from_radius(start, end, r.to_mm(unit), ccw)
            }
            ArcCenter::Offset { i, j } => [start[0] + i.to_mm(unit), start[1] + j.to_mm(unit)],
        }
    }
}

impl Display for Command {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::FormatCode(fmt) => writeln!(f, "FMAT,{}", fmt),
            Command::Incremental(true) => writeln!(f, "ICI,ON"),
            Command::Incremental(false) => writeln!(f, "ICI,OFF"),
            Command::UnitDefinition(ud) => writeln!(f, "{}", ud),
            Command::Geometric(code) => writeln!(f, "{}", code),
            Command::Machine(code) => writeln!(f, "{}", code),
            Command::Coordinate(x, y, fmt) => {
                if let Some(x) = x {
                    write!(f, "X{}", fmt.serialize(*x))?
                };
                if let Some(y) = y {
                    write!(f, "Y{}", fmt.serialize(*y))?
                };
                if x.is_some() || y.is_some() {
                    writeln!(f)?;
                }
                Ok(())
            }
            Command::Tool(id) => writeln!(f, "T{}", id),
            Command::ToolDefinition(td) => writeln!(f, "T{}C{:0.3}", td.tool_number, td.diameter),
            Command::Comment(c) => writeln!(f, ";{}", c),
            Command::FeedRate(rate) => writeln!(f, "F{}", rate),
            Command::Slot {
                from_x,
                from_y,
                to_x,
                to_y,
                fmt,
            } => {
                if let Some(x) = from_x {
                    write!(f, "X{}", fmt.serialize(*x))?
                };
                if let Some(y) = from_y {
                    write!(f, "Y{}", fmt.serialize(*y))?
                };
                write!(f, "G85")?;
                if let Some(x) = to_x {
                    write!(f, "X{}", fmt.serialize(*x))?
                };
                if let Some(y) = to_y {
                    write!(f, "Y{}", fmt.serialize(*y))?
                };
                writeln!(f)
            }
            Command::Arc { x, y, center, fmt } => {
                if let Some(x) = x {
                    write!(f, "X{}", fmt.serialize(*x))?
                };
                if let Some(y) = y {
                    write!(f, "Y{}", fmt.serialize(*y))?
                };
                match center {
                    ArcCenter::Radius(r) => write!(f, "A{}", fmt.serialize(*r))?,
                    ArcCenter::Offset { i, j } => {
                        write!(f, "I{}J{}", fmt.serialize(*i), fmt.serialize(*j))?
                    }
                }
                writeln!(f)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnitDefinition {
    pub unit: Unit,
    ty: ZeroSuppression,
    leading: u8,
    trailing: u8,
    /// Decimal-point coordinate mode (XNC): coords written as floats, no zero suppression.
    decimal: bool,
}

impl Default for UnitDefinition {
    fn default() -> Self {
        Self::default_for(Unit::Inches)
    }
}

impl UnitDefinition {
    /// Per-unit defaults per Excellon 2 / IPC-NC-349 (INCH 2.4 LZ, METRIC 3.3 LZ).
    fn default_for(unit: Unit) -> Self {
        let (leading, trailing) = match unit {
            Unit::Inches => (2, 4),
            Unit::Millimeters => (3, 3),
        };
        Self {
            unit,
            ty: ZeroSuppression::Leading,
            leading,
            trailing,
            decimal: false,
        }
    }
}
impl Display for UnitDefinition {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let ty = match self.unit {
            Unit::Inches => "INCH",
            Unit::Millimeters => "METRIC",
        };
        if self.decimal {
            write!(f, "{}", ty)
        } else {
            write!(
                f,
                "{},{},{}.{}",
                ty,
                self.ty,
                "0".repeat(self.leading as usize),
                "0".repeat(self.trailing as usize)
            )
        }
    }
}

impl UnitDefinition {
    fn parse_num(&self, raw: &str) -> Result<f64, ParseFloatError> {
        if self.decimal || raw.contains('.') {
            return raw.parse::<f64>();
        }
        let neg = raw.starts_with('-');
        let len = (self.trailing + self.leading) as usize;
        let raw = if self.ty == ZeroSuppression::Leading {
            let (raw, prefix) = if neg { (&raw[1..], "-") } else { (raw, "") };
            if raw.len() < len {
                format!("{}{}{}", prefix, raw, "0".repeat(len - raw.len()))
            } else {
                format!("{}{}", prefix, raw)
            }
        } else {
            raw.to_string()
        };
        raw.parse::<f64>()
            .map(|t| t / 10f64.powi(self.trailing as i32))
    }

    fn serialize(&self, num: f64) -> String {
        // Always emit an explicit decimal point. Zero-suppressed integer
        // coordinates (a bare "1" meaning 10.0 or 0.0001 depending on the
        // format header) are mis-read by many drill programs; an explicit
        // point is self-describing and honored by every reader regardless of
        // the declared LZ/TZ format. `parse_num` still reads legacy
        // zero-suppressed input on the way in.
        //
        // Round to the format's resolution (decimal/XNC uses 6 places to strip
        // binary-float noise like 0.19999999998 -> 0.2), then trim redundant
        // trailing zeros while keeping at least one fractional digit ("10.0").
        let decimals = if self.decimal {
            6
        } else {
            (self.trailing as usize).max(1)
        };
        let mut s = format!("{:.*}", decimals, num);
        let dot = s.find('.').expect("decimals >= 1 guarantees a point");
        let last_nonzero = s.rfind(|c| c != '0').expect("string is non-empty");
        s.truncate(last_nonzero.max(dot + 1) + 1);
        s
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Display)]
pub enum ZeroSuppression {
    #[display("LZ")]
    Leading,
    #[display("TZ")]
    Trailing,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GeometricCode {
    Mode(Mode),
    // Sleep time in seconds
    VariableDwell(u16),             //G04X#
    OverrideFeed,                   // G07
    InputMode(InputMode),           // G90, G91
    CutterCompensation(CutterComp), // G40, G41, G42
}

#[derive(Debug, Clone, Eq, PartialEq, Display)]
pub enum CutterComp {
    #[display("G40")]
    Off,
    #[display("G41")]
    Left,
    #[display("G42")]
    Right,
}

impl Display for GeometricCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            GeometricCode::Mode(mode) => write!(f, "{}", mode),
            GeometricCode::VariableDwell(time) => write!(f, "G04X{}", time),
            GeometricCode::OverrideFeed => write!(f, "G07"),
            GeometricCode::InputMode(t) => t.fmt(f),
            GeometricCode::CutterCompensation(c) => write!(f, "{}", c),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Display)]
pub enum InputMode {
    #[display("G90")]
    Absolute,
    #[display("G91")]
    Incremental,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Display)]
pub enum Mode {
    #[display("G00")]
    Route,
    #[display("G01")]
    Linear,
    #[display("G02")]
    CircularCW,
    #[display("G03")]
    CircularCWW,
    #[display("G05")]
    DrillMode,
    #[display("G81")]
    CannedDrill,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    tool_number: u32,
    diameter: f64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum MachineCode {
    EndOfProgram, // M30
    HeaderStart,  // M48
    Scale(Unit),  // M71, M72
    HeaderEnd,    // M95
    RewindStop,   // %
    RouterDown,   // M15
    RouterUp,     // M17
}

impl Display for MachineCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            MachineCode::EndOfProgram => write!(f, "M30"),
            MachineCode::HeaderStart => write!(f, "M48"),
            MachineCode::Scale(Unit::Millimeters) => write!(f, "M71"),
            MachineCode::Scale(Unit::Inches) => write!(f, "M72"),
            MachineCode::HeaderEnd => write!(f, "M95"),
            MachineCode::RewindStop => write!(f, "%"),
            MachineCode::RouterDown => write!(f, "M15"),
            MachineCode::RouterUp => write!(f, "M17"),
        }
    }
}

enum LineResult {
    Command(Command),
    RawCoordinate(Option<f64>, Option<f64>),
    RawArc(Option<f64>, Option<f64>, ArcCenter),
    /// `R<n>X<dx>Y<dy>`: repeat the last hit `n` times, each offset by `(dx, dy)`.
    Repeat(u32, Option<f64>, Option<f64>),
}

fn run<'i, T, P>(input: &'i str, mut parser: P) -> Result<T, ()>
where
    P: Parser<&'i str, T, ContextError>,
{
    parser.parse(input).map_err(|_| ())
}

// Excellon writes tool and G-code numbers zero-padded (`04`, `00`), which
// winnow's `dec_uint` rejects; the call sites use `digit1.parse_to()` instead.

fn parse_fmat(tail: &str) -> Result<LineResult, ExcellonError> {
    run(tail, digit1.parse_to::<u8>())
        .map(|v| LineResult::Command(Command::FormatCode(v)))
        .map_err(|_| ExcellonError::InvalidVersion)
}

fn parse_ici(tail: &str) -> Result<LineResult, ExcellonError> {
    // A bare `ICI` (no option) means incremental input is on.
    if tail.trim().is_empty() {
        return Ok(LineResult::Command(Command::Incremental(true)));
    }
    let opts = preceded(',', alt(("ON", "OFF")));
    match run(tail, opts) {
        Ok("ON") => Ok(LineResult::Command(Command::Incremental(true))),
        Ok("OFF") => Ok(LineResult::Command(Command::Incremental(false))),
        _ => Err(ExcellonError::InvalidCicOption(tail.to_string())),
    }
}

fn parse_unit(line: &str) -> Result<LineResult, ExcellonError> {
    let unit = alt((
        "METRIC".value(Unit::Millimeters),
        "INCH".value(Unit::Inches),
    ));
    let ty = opt(preceded(
        ',',
        alt((
            "TZ".value(ZeroSuppression::Trailing),
            "LZ".value(ZeroSuppression::Leading),
        )),
    ));
    let digits = opt(preceded(
        ',',
        (digit1, '.', digit1).map(|(l, _, t): (&str, _, &str)| (l.len() as u8, t.len() as u8)),
    ));
    let (unit, ty_opt, digits_opt) =
        run(line, (unit, ty, digits)).map_err(|_| ExcellonError::InvalidUnitDefinition)?;
    // XNC form: `INCH` / `METRIC` alone → decimal coordinates, no zero suppression.
    let decimal = ty_opt.is_none() && digits_opt.is_none();
    let default = UnitDefinition::default_for(unit);
    let (leading, trailing) = digits_opt.unwrap_or((default.leading, default.trailing));
    Ok(LineResult::Command(Command::UnitDefinition(
        UnitDefinition {
            ty: ty_opt.unwrap_or(default.ty),
            leading,
            trailing,
            decimal,
            ..default
        },
    )))
}

fn parse_machine(tail: &str, line: &str) -> Result<LineResult, ExcellonError> {
    let code = run(tail, digit1.parse_to::<u8>())
        .map_err(|_| ExcellonError::InvalidCmd(line.to_string()))?;
    let mc = match code {
        15 => MachineCode::RouterDown,
        // M16 (retract with clamping) and M17 (retract without) both lift the
        // router; neither draws further route segments.
        16 | 17 => MachineCode::RouterUp,
        30 => MachineCode::EndOfProgram,
        48 => MachineCode::HeaderStart,
        71 => MachineCode::Scale(Unit::Millimeters),
        72 => MachineCode::Scale(Unit::Inches),
        95 => MachineCode::HeaderEnd,
        c => return Err(ExcellonError::InvalidMachineCode(c)),
    };
    Ok(LineResult::Command(Command::Machine(mc)))
}

fn parse_tool(tail: &str, line: &str) -> Result<LineResult, ExcellonError> {
    // `T<n>` selects a tool. A definition carries parameters in any order, e.g.
    // `T1C0.8` (KiCad) or `T01F00S00C0.8` (Altium: feed, speed, diameter);
    // only the diameter `C` is kept.
    let param = (one_of(['B', 'C', 'F', 'H', 'S', 'Z']), float::<_, f64, _>);
    let parser = (
        digit1.parse_to::<u32>(),
        repeat::<_, _, Vec<_>, _, _>(0.., param),
    );
    match run(tail, parser) {
        Ok((tool_number, params)) => Ok(LineResult::Command(
            match params.iter().find(|(p, _)| *p == 'C') {
                Some((_, diameter)) => Command::ToolDefinition(ToolDefinition {
                    tool_number,
                    diameter: *diameter,
                }),
                None => Command::Tool(tool_number),
            },
        )),
        Err(_) => Err(ExcellonError::InvalidToolDefinition(line.to_string())),
    }
}

fn parse_repeat(tail: &str, line: &str, fmt: &UnitDefinition) -> Result<LineResult, ExcellonError> {
    let (count, offset) = run(tail, (digit1.parse_to::<u32>(), rest))
        .map_err(|_| ExcellonError::InvalidCmd(line.to_string()))?;
    let (dx, dy) = parse_xy(offset, fmt)?;
    Ok(LineResult::Repeat(count, dx, dy))
}

fn parse_geometric(tail: &str, line: &str) -> Result<LineResult, ExcellonError> {
    let id_p = take(2usize).and_then(digit1.parse_to::<u8>());
    let dwell_p = opt(preceded('X', digit1.parse_to::<u16>()));
    let (id, dwell) =
        run(tail, (id_p, dwell_p)).map_err(|_| ExcellonError::InvalidCmd(line.to_string()))?;
    let code = match id {
        0 => GeometricCode::Mode(Mode::Route),
        1 => GeometricCode::Mode(Mode::Linear),
        2 => GeometricCode::Mode(Mode::CircularCW),
        3 => GeometricCode::Mode(Mode::CircularCWW),
        4 => GeometricCode::VariableDwell(
            dwell.ok_or_else(|| ExcellonError::Custom("Does not start with `G04X`".to_string()))?,
        ),
        5 => GeometricCode::Mode(Mode::DrillMode),
        7 => GeometricCode::OverrideFeed,
        40 => GeometricCode::CutterCompensation(CutterComp::Off),
        41 => GeometricCode::CutterCompensation(CutterComp::Left),
        42 => GeometricCode::CutterCompensation(CutterComp::Right),
        81 => GeometricCode::Mode(Mode::CannedDrill),
        90 => GeometricCode::InputMode(InputMode::Absolute),
        91 => GeometricCode::InputMode(InputMode::Incremental),
        c => return Err(ExcellonError::InvalidGeometricCode(c)),
    };
    Ok(LineResult::Command(Command::Geometric(code)))
}

// Parse an `X<n>Y<n>` coordinate fragment. Either axis may be absent; an
// empty fragment yields `(None, None)` so a missing slot endpoint can carry
// over the current position.
fn parse_xy(
    fragment: &str,
    fmt: &UnitDefinition,
) -> Result<(Option<f64>, Option<f64>), ExcellonError> {
    if fragment.is_empty() {
        return Ok((None, None));
    }
    fn inner<'i>(input: &mut &'i str) -> ModalResult<(Option<&'i str>, Option<&'i str>)> {
        let val = |i: &mut &'i str| (opt('-'), digit1, opt(('.', digit1))).take().parse_next(i);
        (opt(preceded('X', val)), opt(preceded('Y', val))).parse_next(input)
    }
    match inner.parse(fragment) {
        Ok((x, y)) if x.is_some() || y.is_some() => Ok((
            x.and_then(|t| fmt.parse_num(t).ok()),
            y.and_then(|t| fmt.parse_num(t).ok()),
        )),
        _ => Err(ExcellonError::InvalidCoordinate(fragment.to_string())),
    }
}

// Parse a coordinate line, which in circular mode may carry `A<r>` or
// `I<i>J<j>` after the end point.
fn parse_coord(line: &str, fmt: &UnitDefinition) -> Result<LineResult, ExcellonError> {
    let Some(at) = line.find(['A', 'I', 'J']) else {
        let (x, y) = parse_xy(line, fmt)?;
        return Ok(LineResult::RawCoordinate(x, y));
    };
    let (x, y) = parse_xy(&line[..at], fmt)?;
    fn inner<'i>(input: &mut &'i str) -> ModalResult<(char, &'i str, Option<&'i str>)> {
        let val = |i: &mut &'i str| (opt('-'), digit1, opt(('.', digit1))).take().parse_next(i);
        alt((
            ('A', val, winnow::combinator::empty.value(None)),
            ('I', val, opt(preceded('J', val))),
            ('J', val, winnow::combinator::empty.value(None)),
        ))
        .parse_next(input)
    }
    let invalid = || ExcellonError::InvalidCoordinate(line.to_string());
    let (kind, first, second) = inner.parse(&line[at..]).map_err(|_| invalid())?;
    let num = |t: &str| fmt.parse_num(t).map_err(|_| invalid());
    let center = match kind {
        'A' => ArcCenter::Radius(num(first)?),
        'I' => ArcCenter::Offset {
            i: num(first)?,
            j: second.map(num).transpose()?.unwrap_or(0.0),
        },
        _ => ArcCenter::Offset {
            i: 0.0,
            j: num(first)?,
        },
    };
    Ok(LineResult::RawArc(x, y, center))
}

fn parse_feed(tail: &str, line: &str) -> Result<LineResult, ExcellonError> {
    run(tail, digit1.parse_to::<u32>())
        .map(|rate| LineResult::Command(Command::FeedRate(rate)))
        .map_err(|_| ExcellonError::InvalidCmd(line.to_string()))
}

// Parse a `G`-prefixed line. Most G-codes stand alone, but routing files pack
// a coordinate onto the same line (`G00X111Y3114`, `G01Y9019`); those yield
// both the geometric command and the coordinate. `G04X<n>` keeps its dwell
// argument and is parsed whole.
fn parse_geometric_line(
    line: &str,
    fmt: &UnitDefinition,
) -> Result<Vec<LineResult>, ExcellonError> {
    let digits_end = line[1..]
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(line.len());
    let (gpart, rest) = line.split_at(digits_end);
    // `G04X<n>` keeps its trailing X as a dwell argument, not a coordinate.
    if gpart == "G04" || rest.is_empty() {
        return Ok(vec![parse_geometric(&line[1..], line)?]);
    }
    Ok(vec![
        parse_geometric(&gpart[1..], gpart)?,
        parse_coord(rest, fmt)?,
    ])
}

fn parse_line(line: &str, fmt: &UnitDefinition) -> Result<Vec<LineResult>, ExcellonError> {
    if let Some(stripped) = line.strip_prefix(';') {
        return Ok(vec![LineResult::Command(Command::Comment(
            stripped.to_string(),
        ))]);
    }
    if line == "%" {
        return Ok(vec![LineResult::Command(Command::Machine(
            MachineCode::RewindStop,
        ))]);
    }
    if let Some(tail) = line.strip_prefix("FMAT,") {
        return Ok(vec![parse_fmat(tail)?]);
    }
    if let Some(tail) = line.strip_prefix("ICI") {
        return Ok(vec![parse_ici(tail)?]);
    }
    if line.starts_with("METRIC") || line.starts_with("INCH") {
        return Ok(vec![parse_unit(line)?]);
    }
    // `[X<a>Y<b>]G85X<c>Y<d>` — a routed slot (canned milling cycle).
    if let Some((before, after)) = line.split_once("G85") {
        let (from_x, from_y) = parse_xy(before, fmt)?;
        let (to_x, to_y) = parse_xy(after, fmt)?;
        return Ok(vec![LineResult::Command(Command::Slot {
            from_x,
            from_y,
            to_x,
            to_y,
            fmt: fmt.clone(),
        })]);
    }
    if let Some(tail) = line.strip_prefix('M') {
        return Ok(vec![parse_machine(tail, line)?]);
    }
    if let Some(tail) = line.strip_prefix('T') {
        return Ok(vec![parse_tool(tail, line)?]);
    }
    if let Some(tail) = line.strip_prefix('R') {
        return Ok(vec![parse_repeat(tail, line, fmt)?]);
    }
    if let Some(tail) = line.strip_prefix('F') {
        return Ok(vec![parse_feed(tail, line)?]);
    }
    if line.starts_with('G') {
        return parse_geometric_line(line, fmt);
    }
    if line.starts_with('X') || line.starts_with('Y') {
        return Ok(vec![parse_coord(line, fmt)?]);
    }
    Err(ExcellonError::InvalidCmd(line.to_string()))
}

pub fn parse_excellon<T>(mut reader: BufReader<T>) -> std::io::Result<ExcellonLayerData>
where
    T: std::io::Read,
{
    let mut commands = Vec::new();

    let mut format = UnitDefinition::default();
    let mut unit_set = false;

    let mut buf = String::new();
    let mut line_number = 0;
    let mut tools = HashMap::new();
    // Indices of coordinates expanded from `R` repeat codes; they are deltas
    // from the previous hit regardless of the input mode.
    let mut repeat_steps = HashSet::new();
    while reader.read_line(&mut buf)? > 0 {
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            buf.clear();
            line_number += 1;
            continue;
        }
        // A single source line may yield several commands (e.g. a G-code
        // with a coordinate, `G00X111Y3114`); each is recorded separately.
        match parse_line(trimmed, &format) {
            Ok(results) => {
                for result in results {
                    let cmd_result = match result {
                        LineResult::Repeat(count, dx, dy) => {
                            if !unit_set {
                                commands.push(Err(ExcellonParseFormat {
                                    source: ExcellonError::CoordinateBeforeUnit,
                                    line: line_number,
                                    content: trimmed.to_string(),
                                }));
                                continue;
                            }
                            for _ in 0..count {
                                repeat_steps.insert(commands.len());
                                commands.push(Ok(Command::Coordinate(dx, dy, format.clone())));
                            }
                            continue;
                        }
                        LineResult::Command(cmd) => {
                            match &cmd {
                                Command::UnitDefinition(unit) => {
                                    format = unit.clone();
                                    unit_set = true;
                                }
                                Command::Machine(MachineCode::Scale(u)) => format.unit = *u,
                                Command::ToolDefinition(td) => {
                                    tools.insert(td.tool_number, td.diameter);
                                }
                                Command::Tool(id) if !tools.contains_key(id) => {
                                    return Err(ExcellonError::InvalidToolNumber(*id).into());
                                }
                                _ => {}
                            }
                            Ok(cmd)
                        }
                        LineResult::RawCoordinate(..) | LineResult::RawArc(..) if !unit_set => {
                            Err(ExcellonError::CoordinateBeforeUnit)
                        }
                        LineResult::RawArc(x, y, center) => Ok(Command::Arc {
                            x,
                            y,
                            center,
                            fmt: format.clone(),
                        }),
                        LineResult::RawCoordinate(x, y) => {
                            Ok(Command::Coordinate(x, y, format.clone()))
                        }
                    };
                    commands.push(cmd_result.map_err(|e| ExcellonParseFormat {
                        source: e,
                        line: line_number,
                        content: trimmed.to_string(),
                    }));
                }
            }
            Err(err) => commands.push(Err(ExcellonParseFormat {
                source: err,
                line: line_number,
                content: trimmed.to_string(),
            })),
        }
        buf.clear();
        line_number += 1;
    }

    let mut first_sig = None;
    let mut last_sig = None;
    for cmd in &commands {
        if !matches!(cmd, Ok(Command::Comment(_))) {
            first_sig.get_or_insert(cmd);
            last_sig = Some(cmd);
        }
    }
    if !matches!(
        first_sig,
        Some(Ok(Command::Machine(MachineCode::HeaderStart)))
    ) {
        return Err(ExcellonError::MissingHeaderStart.into());
    }
    if !matches!(
        last_sig,
        Some(Ok(Command::Machine(MachineCode::EndOfProgram)))
    ) {
        return Err(ExcellonError::MissingEndOfProgram.into());
    }
    resolve_absolute_coordinates(&mut commands, &repeat_steps);
    let mut header = commands
        .iter()
        .take_while(|cmd| {
            !matches!(
                cmd,
                Ok(Command::Machine(MachineCode::HeaderEnd))
                    | Ok(Command::Machine(MachineCode::RewindStop))
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    if let Some(cmd) = commands.get(header.len()) {
        header.push(cmd.clone());
    } else {
        return Err(ExcellonError::MissingHeaderEnd.into());
    }
    let commands = commands.into_iter().skip(header.len()).collect::<Vec<_>>();
    let format = header
        .iter()
        .find_map(|cmd| match cmd {
            Ok(Command::UnitDefinition(ud)) => Some(ud.clone()),
            _ => None,
        })
        .unwrap_or(UnitDefinition::default());

    Ok(ExcellonLayerData {
        header,
        commands,
        unit: format,
        tools,
    })
}

impl From<ExcellonLayerData> for LayerData {
    fn from(value: ExcellonLayerData) -> Self {
        LayerData::Excellon(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_excellon() -> Result<(), Box<dyn std::error::Error>> {
        let raw = include_str!("../../test/demo.drd");
        let reader = BufReader::new(Cursor::new(raw));
        let mut data = parse_excellon(reader)?;
        let mut clone = data.clone();
        clone.transform(&Pos { x: 10.0, y: 15.0 });
        data.merge(&clone).unwrap();
        for cmd in data.header {
            cmd?;
        }
        for cmd in data.commands {
            cmd?;
        }
        Ok(())
    }

    #[test]
    fn test_decimal_coords_xnc() -> Result<(), Box<dyn std::error::Error>> {
        let raw = "M48\n\
                   METRIC\n\
                   T01C0.6\n\
                   %\n\
                   G05\n\
                   T01\n\
                   X9.01Y3.3375\n\
                   X-1.5Y2\n\
                   M30\n";
        let data = parse_excellon(BufReader::new(Cursor::new(raw)))?;
        assert!(data.unit.decimal);
        assert_eq!(data.unit.unit, Unit::Millimeters);
        let coords: Vec<_> = data
            .commands
            .iter()
            .filter_map(|c| match c {
                Ok(Command::Coordinate(x, y, _)) => Some((*x, *y)),
                _ => None,
            })
            .collect();
        assert_eq!(
            coords,
            vec![(Some(9.01), Some(3.3375)), (Some(-1.5), Some(2.0))]
        );
        Ok(())
    }

    #[test]
    fn test_routing_program() -> Result<(), Box<dyn std::error::Error>> {
        // G-codes packed with coordinates, cutter compensation, feed rate,
        // router up/down, a canned-cycle switch and a G85 slot.
        let raw = "M48\n\
                   FMAT,1\n\
                   INCH,TZ,00.0000\n\
                   T04C0.0787\n\
                   %\n\
                   T04\n\
                   G00X111Y3114\n\
                   G41\n\
                   F059\n\
                   M15\n\
                   G01Y9019\n\
                   X10938\n\
                   M17\n\
                   G81\n\
                   X30784Y24630G85X30896Y24443\n\
                   M30\n";
        let data = parse_excellon(BufReader::new(Cursor::new(raw)))?;
        for cmd in &data.header {
            cmd.clone()?;
        }
        for cmd in &data.commands {
            cmd.clone()?;
        }
        let count = |pred: fn(&Command) -> bool| {
            data.commands
                .iter()
                .filter(|c| c.as_ref().is_ok_and(pred))
                .count()
        };
        // `G00X111Y3114` and `G01Y9019` each split into a mode + a coordinate.
        assert_eq!(count(|c| matches!(c, Command::Coordinate(..))), 3);
        assert_eq!(count(|c| matches!(c, Command::Slot { .. })), 1);
        assert_eq!(count(|c| matches!(c, Command::FeedRate(_))), 1);
        assert!(
            data.commands
                .iter()
                .any(|c| matches!(c, Ok(Command::Machine(MachineCode::RouterDown))))
        );
        assert!(data.commands.iter().any(|c| matches!(
            c,
            Ok(Command::Geometric(GeometricCode::CutterCompensation(
                CutterComp::Left
            )))
        )));
        // Round-trip: G00 must serialize without a placeholder coordinate.
        let mut out = BufWriter::new(Vec::new());
        data.write_to(&mut out)?;
        let serialized = String::from_utf8(out.into_inner()?)?;
        assert!(serialized.contains("G00\n"), "G00 missing: {serialized}");
        assert!(
            !serialized.contains("G00X"),
            "G00 still carries a stub coord: {serialized}"
        );
        Ok(())
    }

    #[test]
    fn test_scale() -> Result<(), Box<dyn std::error::Error>> {
        let raw = "M48\n\
                   METRIC\n\
                   T01C0.6\n\
                   %\n\
                   G05\n\
                   T01\n\
                   X2Y3\n\
                   M30\n";
        let mut data = parse_excellon(BufReader::new(Cursor::new(raw)))?;
        data.scale(2.0, 3.0);
        let coords: Vec<_> = data
            .commands
            .iter()
            .filter_map(|c| match c {
                Ok(Command::Coordinate(x, y, _)) => Some((*x, *y)),
                _ => None,
            })
            .collect();
        assert_eq!(coords, vec![(Some(4.0), Some(9.0))]);
        Ok(())
    }

    #[test]
    fn test_get_corners() -> Result<(), Box<dyn std::error::Error>> {
        let raw = "M48\n\
                   METRIC\n\
                   T01C2.0\n\
                   %\n\
                   G05\n\
                   T01\n\
                   X0Y0\n\
                   X10Y5\n\
                   M30\n";
        let data = parse_excellon(BufReader::new(Cursor::new(raw)))?;
        let (min, max) = data.get_corners();
        // Tool radius 1.0mm expands the hull around (0,0) and (10,5).
        assert_eq!((min.x, min.y), (-1.0, -1.0));
        assert_eq!((max.x, max.y), (11.0, 6.0));
        Ok(())
    }

    #[test]
    fn test_serialize_no_float_noise() {
        // Decimal (XNC) mode: float noise must not leak into output.
        let dec = UnitDefinition {
            decimal: true,
            ..UnitDefinition::default_for(Unit::Millimeters)
        };
        assert_eq!(dec.serialize(0.1 + 0.2), "0.3");
        assert_eq!(dec.serialize(3.3375), "3.3375");
        assert_eq!(dec.serialize(-1.5), "-1.5");
        // Always keep an explicit decimal point with at least one digit.
        assert_eq!(dec.serialize(2.0), "2.0");

        // Fixed-point mode rounds rather than truncates.
        let fmt = UnitDefinition {
            leading: 3,
            trailing: 3,
            ty: ZeroSuppression::Leading,
            unit: Unit::Millimeters,
            decimal: false,
        };
        // 0.2 * 1000 = 199.9999… must round to 200 ("0002" -> trim -> "0002"? LZ trims trailing)
        assert_eq!(fmt.parse_num(&fmt.serialize(0.2)).unwrap(), 0.2);
        assert_eq!(fmt.parse_num(&fmt.serialize(12.34)).unwrap(), 12.34);
    }

    #[test]
    fn test_unit_defaults() {
        let inch = UnitDefinition::default_for(Unit::Inches);
        assert_eq!((inch.leading, inch.trailing), (2, 4));
        assert_eq!(inch.ty, ZeroSuppression::Leading);
        let mm = UnitDefinition::default_for(Unit::Millimeters);
        assert_eq!((mm.leading, mm.trailing), (3, 3));
        assert_eq!(mm.ty, ZeroSuppression::Leading);
    }

    fn excellon_err(err: std::io::Error) -> ExcellonError {
        err.into_inner()
            .and_then(|e| e.downcast::<ExcellonError>().ok())
            .map(|b| *b)
            .expect("ExcellonError")
    }

    #[test]
    fn test_missing_header_start() {
        let raw = "METRIC\nM30\n";
        let err = parse_excellon(BufReader::new(Cursor::new(raw))).unwrap_err();
        assert!(matches!(
            excellon_err(err),
            ExcellonError::MissingHeaderStart
        ));
    }

    #[test]
    fn test_missing_end_of_program() {
        let raw = "M48\nMETRIC\n%\nG05\n";
        let err = parse_excellon(BufReader::new(Cursor::new(raw))).unwrap_err();
        assert!(matches!(
            excellon_err(err),
            ExcellonError::MissingEndOfProgram
        ));
    }

    #[test]
    fn test_leading_trailing() -> Result<(), Box<dyn std::error::Error>> {
        let fmt = UnitDefinition {
            leading: 3,
            trailing: 3,
            ty: ZeroSuppression::Leading,
            unit: Unit::Millimeters,
            decimal: false,
        };
        // Output is always explicit-decimal regardless of LZ/TZ format, but
        // the reader still parses legacy zero-suppressed coordinates.
        assert_eq!(fmt.serialize(12.34), "12.34");
        assert_eq!(fmt.serialize(-12.34), "-12.34");
        assert_eq!(fmt.parse_num("01234")?, 12.34); // LZ-suppressed input
        assert_eq!(fmt.parse_num("-01234")?, -12.34);
        let fmt = UnitDefinition {
            leading: 3,
            trailing: 3,
            ty: ZeroSuppression::Trailing,
            unit: Unit::Millimeters,
            decimal: false,
        };
        assert_eq!(fmt.serialize(12.34), "12.34");
        assert_eq!(fmt.serialize(-12.34), "-12.34");
        assert_eq!(fmt.parse_num("12340")?, 12.34); // TZ-suppressed input
        assert_eq!(fmt.parse_num("-12340")?, -12.34);
        Ok(())
    }

    fn coords(data: &ExcellonLayerData) -> Vec<(Option<f64>, Option<f64>)> {
        data.commands
            .iter()
            .filter_map(|c| match c {
                Ok(Command::Coordinate(x, y, _)) => Some((*x, *y)),
                _ => None,
            })
            .collect()
    }

    fn assert_coords(actual: &[(Option<f64>, Option<f64>)], expected: &[(f64, f64)]) {
        assert_eq!(actual.len(), expected.len(), "{actual:?} vs {expected:?}");
        for (a, e) in actual.iter().zip(expected) {
            let (Some(ax), Some(ay)) = *a else {
                panic!("axis left out: {actual:?}");
            };
            assert!(
                (ax - e.0).abs() < 1e-9 && (ay - e.1).abs() < 1e-9,
                "{actual:?} vs {expected:?}"
            );
        }
    }

    /// An omitted axis is modal in absolute mode; rotating must fill it from the
    /// current point and write both axes.
    #[test]
    fn test_rotate_fills_modal_axis() -> Result<(), Box<dyn std::error::Error>> {
        let raw = "M48\nMETRIC\nT01C0.6\n%\nG05\nT01\nX1.0Y2.0\nX3.0\nY4.0\nM30\n";
        let mut data = parse_excellon(BufReader::new(Cursor::new(raw)))?;
        data.rebase(1, &Pos { x: 0.0, y: 0.0 });
        assert_coords(&coords(&data), &[(2.0, -1.0), (2.0, -3.0), (4.0, -3.0)]);
        Ok(())
    }

    /// Incremental programs are resolved to absolute on load, so translating
    /// and rotating them treats every point the same way.
    #[test]
    fn test_incremental_resolved_on_load() -> Result<(), Box<dyn std::error::Error>> {
        let raw = "M48\nMETRIC\nICI\nT01C0.6\n%\nG05\nT01\nX1.0\nX3.0Y1.0\nG90\nX0.5\nM30\n";
        let mut data = parse_excellon(BufReader::new(Cursor::new(raw)))?;
        assert_coords(&coords(&data), &[(1.0, 0.0), (4.0, 1.0), (0.5, 1.0)]);
        assert!(
            !data
                .header
                .iter()
                .any(|c| matches!(c, Ok(Command::Incremental(true))))
        );
        data.transform(&Pos { x: 10.0, y: 5.0 });
        assert_coords(&coords(&data), &[(11.0, 5.0), (14.0, 6.0), (10.5, 6.0)]);
        Ok(())
    }

    /// Altium writes tool definitions with feed and speed before the diameter.
    #[test]
    fn test_altium_tool_definition() -> Result<(), Box<dyn std::error::Error>> {
        let raw = "M48\nMETRIC,LZ,000.000\nT01F00S00C0.80\nT02C1.2F100\n%\nG05\nT01\nX1.0Y1.0\nT02\nX2.0Y2.0\nM30\n";
        let data = parse_excellon(BufReader::new(Cursor::new(raw)))?;
        assert_eq!(data.tools.get(&1), Some(&0.8));
        assert_eq!(data.tools.get(&2), Some(&1.2));
        for cmd in data.header.iter().chain(&data.commands) {
            cmd.clone()?;
        }
        Ok(())
    }

    /// M16 (retract with clamping) lifts the router like M17.
    #[test]
    fn test_m16_router_up() -> Result<(), Box<dyn std::error::Error>> {
        let raw = "M48\nMETRIC\nT01C0.6\n%\nT01\nG00X1.0Y1.0\nM15\nG01X2.0\nM16\nG05\nM30\n";
        let data = parse_excellon(BufReader::new(Cursor::new(raw)))?;
        assert!(
            data.commands
                .iter()
                .any(|c| matches!(c, Ok(Command::Machine(MachineCode::RouterUp))))
        );
        for cmd in &data.commands {
            cmd.clone()?;
        }
        Ok(())
    }

    /// `R<n>X<dx>Y<dy>` repeats the previous hit n times with an offset.
    #[test]
    fn test_repeat_code() -> Result<(), Box<dyn std::error::Error>> {
        let raw = "M48\nMETRIC\nT01C0.6\n%\nG05\nT01\nX1.0Y1.0\nR3X0.5\nX10.0Y10.0\nM30\n";
        let data = parse_excellon(BufReader::new(Cursor::new(raw)))?;
        for cmd in &data.commands {
            cmd.clone()?;
        }
        assert_coords(
            &coords(&data),
            &[(1.0, 1.0), (1.5, 1.0), (2.0, 1.0), (2.5, 1.0), (10.0, 10.0)],
        );
        Ok(())
    }

    /// Circular routes keep their radius / centre offset through parse,
    /// rotation and write.
    #[test]
    fn test_arc_routes() -> Result<(), Box<dyn std::error::Error>> {
        let raw = "M48\nMETRIC\nT01C0.6\n%\nT01\nG00X1.0Y0.0\nM15\nG03X0.0Y1.0A1.0\n\
                   G02X1.0Y2.0I1.0J0.0\nX2.0Y1.0J-1.0\nM17\nM30\n";
        let mut data = parse_excellon(BufReader::new(Cursor::new(raw)))?;
        for cmd in &data.commands {
            cmd.clone()?;
        }
        let arcs = |d: &ExcellonLayerData| -> Vec<ArcCenter> {
            d.commands
                .iter()
                .filter_map(|c| match c {
                    Ok(Command::Arc { center, .. }) => Some(*center),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            arcs(&data),
            [
                ArcCenter::Radius(1.0),
                ArcCenter::Offset { i: 1.0, j: 0.0 },
                ArcCenter::Offset { i: 0.0, j: -1.0 },
            ]
        );
        let mut out = BufWriter::new(Vec::new());
        data.write_to(&mut out)?;
        let text = String::from_utf8(out.into_inner()?)?;
        assert!(text.contains("X0.0Y1.0A1.0"), "{text}");
        assert!(text.contains("X1.0Y2.0I1.0J0.0"), "{text}");

        // One CW quarter turn: (i, j) -> (j, -i).
        data.rebase(1, &Pos::default());
        assert_eq!(arcs(&data)[1], ArcCenter::Offset { i: 0.0, j: -1.0 });
        let (min, max) = parse_excellon(BufReader::new(Cursor::new(raw)))?.get_corners();
        // Arcs around (0,0) and (1,1) stay within their end points; the
        // 0.6 mm tool adds 0.3 mm on every side.
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(near(min.x, -0.3) && near(min.y, -0.3), "{min:?}");
        assert!(near(max.x, 2.3) && near(max.y, 2.3), "{max:?}");
        Ok(())
    }
}
