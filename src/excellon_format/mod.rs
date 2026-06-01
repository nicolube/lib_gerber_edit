use crate::unit_able::UnitAble;
use crate::{
    LayerCorners, LayerData, LayerMerge, LayerScale, LayerStepAndRepeat, LayerTransform, Pos,
};
use derive_more::{Display, Error};
use gerber_parser::gerber_types::Unit;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::num::{ParseFloatError, ParseIntError};
use winnow::ModalResult;
use winnow::Parser;
use winnow::ascii::{digit1, float};
use winnow::combinator::{alt, opt, preceded};
use winnow::error::ContextError;
use winnow::token::take;

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
                Ok(Command::Coordinate(_, _, _)) | Ok(Command::Slot { .. })
            )
        })
    }
}

impl LayerTransform for ExcellonLayerData {
    fn transform(&mut self, transform: &Pos) {
        let mut commands = Vec::new();
        commands.extend(self.header.iter_mut().filter_map(|x| x.as_mut().ok()));
        commands.extend(self.commands.iter_mut().filter_map(|x| x.as_mut().ok()));
        let shift = |v: &mut Option<f64>, delta: f64, unit: &Unit| {
            *v = v.to_mm(unit).map(|n| n + delta).mm_to_unit(unit);
        };
        commands.iter_mut().for_each(|cmd| match cmd {
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
            _ => {}
        })
    }
}

impl LayerMerge for ExcellonLayerData {
    fn merge(&mut self, other: &Self) {
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
            Ok(Command::Geometric(GeometricCode::Mode(m))) => Some(m.clone()),
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
                Ok(Command::Slot { fmt, .. }) => {
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
                        last_mode = Some(m.clone());
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
                self.merge(&copy);
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
        let mut current = Pos::default();
        let mut incremental = self
            .header
            .iter()
            .rev()
            .find_map(|x| match x {
                Ok(Command::Incremental(i)) => Some(*i),
                _ => None,
            })
            .unwrap_or(false);

        for command in self.commands.iter().flatten() {
            match command {
                Command::Tool(id) => {
                    radius = self
                        .tools
                        .get(id)
                        .map(|d| d.to_mm(&self.unit.unit) / 2.0)
                        .unwrap_or(0.0);
                }
                Command::Geometric(GeometricCode::InputMode(im)) => {
                    incremental = *im == InputMode::Incremental;
                }
                Command::Coordinate(x, y, fmt) => {
                    let cx = x.map(|v| v.to_mm(&fmt.unit));
                    let cy = y.map(|v| v.to_mm(&fmt.unit));
                    if incremental {
                        current.x += cx.unwrap_or(0.0);
                        current.y += cy.unwrap_or(0.0);
                    } else {
                        current.x = cx.unwrap_or(current.x);
                        current.y = cy.unwrap_or(current.y);
                    }
                    min.x = min.x.min(current.x - radius);
                    min.y = min.y.min(current.y - radius);
                    max.x = max.x.max(current.x + radius);
                    max.y = max.y.max(current.y + radius);
                }
                Command::Slot {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                    fmt,
                } => {
                    // Both endpoints bound the milled slot; tool width expands it.
                    let from = Pos {
                        x: from_x.map(|v| v.to_mm(&fmt.unit)).unwrap_or(current.x),
                        y: from_y.map(|v| v.to_mm(&fmt.unit)).unwrap_or(current.y),
                    };
                    current.x = to_x.map(|v| v.to_mm(&fmt.unit)).unwrap_or(current.x);
                    current.y = to_y.map(|v| v.to_mm(&fmt.unit)).unwrap_or(current.y);
                    for p in [&from, &current] {
                        min.x = min.x.min(p.x - radius);
                        min.y = min.y.min(p.y - radius);
                        max.x = max.x.max(p.x + radius);
                        max.y = max.y.max(p.y + radius);
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
                    write!(f, "X{}", &fmt.serialize(*x))?
                };
                if let Some(y) = y {
                    write!(f, "Y{}", &fmt.serialize(*y))?
                };
                if x.is_some() || y.is_some() {
                    writeln!(f)?;
                }
                Ok(())
            }
            Command::Tool(id) => writeln!(f, "T{}", id),
            Command::ToolDefinition(td) => writeln!(f, "T{}C{:0.3}", &td.tool_number, &td.diameter),
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
                    write!(f, "X{}", &fmt.serialize(*x))?
                };
                if let Some(y) = from_y {
                    write!(f, "Y{}", &fmt.serialize(*y))?
                };
                write!(f, "G85")?;
                if let Some(x) = to_x {
                    write!(f, "X{}", &fmt.serialize(*x))?
                };
                if let Some(y) = to_y {
                    write!(f, "Y{}", &fmt.serialize(*y))?
                };
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
        if self.decimal {
            // Decimal (XNC) keeps full precision; round to 6 places to strip
            // binary-float noise (e.g. 0.19999999998 -> 0.2) then trim zeros.
            let s = format!("{:.6}", num);
            let s = s.trim_end_matches('0').trim_end_matches('.');
            return s.to_string();
        }
        if num == 0.0 {
            return "0".to_string();
        }
        let num = (num * (10i32.pow(self.trailing as u32) as f64)).round();
        let fmt = format!(
            "{:0a$}",
            num.abs() as isize,
            a = (self.leading + self.trailing) as usize
        );
        format!(
            "{}{}",
            if num < 0.0 { "-" } else { "" },
            if self.ty == ZeroSuppression::Trailing {
                fmt.trim_start_matches('0')
            } else {
                fmt.trim_end_matches('0')
            }
        )
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

#[derive(Debug, Clone, Eq, PartialEq, Display)]
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
        17 => MachineCode::RouterUp,
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
    let parser = (
        digit1.parse_to::<u32>(),
        opt(preceded('C', float::<_, f64, _>)),
    );
    match run(tail, parser) {
        Ok((tool_number, Some(diameter))) => Ok(LineResult::Command(Command::ToolDefinition(
            ToolDefinition {
                tool_number,
                diameter,
            },
        ))),
        Ok((tool_number, None)) => Ok(LineResult::Command(Command::Tool(tool_number))),
        Err(_) => Err(ExcellonError::InvalidToolDefinition(line.to_string())),
    }
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

fn parse_coord(line: &str, fmt: &UnitDefinition) -> Result<LineResult, ExcellonError> {
    let (x, y) = parse_xy(line, fmt)?;
    Ok(LineResult::RawCoordinate(x, y))
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
                                Command::Tool(id) => {
                                    if !tools.contains_key(id) {
                                        return Err(ExcellonError::InvalidToolNumber(*id).into());
                                    }
                                }
                                _ => {}
                            }
                            Ok(cmd)
                        }
                        LineResult::RawCoordinate(_, _) if !unit_set => {
                            Err(ExcellonError::CoordinateBeforeUnit)
                        }
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
        data.merge(&clone);
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
        assert_eq!(dec.serialize(2.0), "2");

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
        let num = 12.34;
        let serialized = fmt.serialize(num);
        assert_eq!(serialized, "01234");
        assert_eq!(fmt.parse_num(&serialized)?, num);
        let num = -12.34;
        let serialized = fmt.serialize(num);
        assert_eq!(serialized, "-01234");
        assert_eq!(fmt.parse_num(&serialized)?, num);
        let fmt = UnitDefinition {
            leading: 3,
            trailing: 3,
            ty: ZeroSuppression::Trailing,
            unit: Unit::Millimeters,
            decimal: false,
        };
        let num = 12.34;
        let serialized = fmt.serialize(num);
        assert_eq!(serialized, "12340");
        assert_eq!(fmt.parse_num(&serialized)?, num);
        let num = -12.34;
        let serialized = fmt.serialize(num);
        assert_eq!(serialized, "-12340");
        assert_eq!(fmt.parse_num(&serialized)?, num);
        Ok(())
    }
}
