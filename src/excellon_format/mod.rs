use crate::excellon_format::Mode::Route;
use crate::unit_able::UnitAble;
use crate::{LayerData, LayerMerge, LayerStepAndRepeat, LayerTransform, Pos};
use derive_more::{Display, Error};
use gerber_parser::gerber_types::Unit;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::num::{ParseFloatError, ParseIntError};
use winnow::ModalResult;
use winnow::Parser;
use winnow::ascii::{dec_uint, digit1, float};
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
        !self
            .commands
            .iter()
            .any(|x| matches!(x, Ok(Command::Coordinate(_, _, _))))
    }
}

impl LayerTransform for ExcellonLayerData {
    fn transform(&mut self, transform: &Pos) {
        let mut commands = Vec::new();
        commands.extend(self.header.iter_mut().filter_map(|x| x.as_mut().ok()));
        commands.extend(self.commands.iter_mut().filter_map(|x| x.as_mut().ok()));
        commands.iter_mut().for_each(|cmd| {
            if let Command::Coordinate(x, y, fmt) = cmd {
                *x = x
                    .to_mm(&fmt.unit)
                    .map(|x| x + transform.x)
                    .mm_to_unit(&fmt.unit);
                *y = y
                    .to_mm(&fmt.unit)
                    .map(|y| y + transform.y)
                    .mm_to_unit(&fmt.unit);
            }
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
    #[display("Failed to parse floating number: {}", _1)]
    FloatParse(#[error(source)] ParseFloatError, String),
    #[display("Failed to parse number: {}", _1)]
    IntParse(#[error(source)] ParseIntError, String),
    #[display("Failed to parse version number")]
    InvalidVersion,
    #[display("{}", _0)]
    Custom(#[error(not(source))] String),
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
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnitDefinition {
    pub unit: Unit,
    ty: ZeroSuppression,
    leading: u8,
    trailing: u8,
}

impl Default for UnitDefinition {
    fn default() -> Self {
        Self {
            unit: Unit::Inches,
            ty: ZeroSuppression::Trailing,
            leading: 3,
            trailing: 3,
        }
    }
}
impl Display for UnitDefinition {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let ty = match self.unit {
            Unit::Inches => "INCH",
            Unit::Millimeters => "METRIC",
        };
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

impl UnitDefinition {
    fn parse_num(&self, raw: &str) -> Result<f64, ParseFloatError> {
        let neg = raw.chars().nth(0) == Some('-');
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
        if neg && raw.len() - 1 > len && raw.len() > len {
            panic!("too many bytes");
        }
        raw.parse::<f64>()
            .map(|t| t / 10f64.powi(self.trailing as i32))
    }

    fn serialize(&self, num: f64) -> String {
        if num == 0.0 {
            return "0".to_string();
        }
        let num = num * (10i32.pow(self.trailing as u32) as f64);
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
    VariableDwell(u16),   //G04X#
    OverrideFeed,         // G07
    InputMode(InputMode), // G90, G91
}


impl Display for GeometricCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            GeometricCode::Mode(mode) => write!(f, "{}", mode),
            GeometricCode::VariableDwell(time) => write!(f, "G04X{}", time),
            GeometricCode::OverrideFeed => write!(f, "G07"),
            GeometricCode::InputMode(t) => t.fmt(f),
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

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Route(f64, f64), //G00
    Linear,          // G01
    CircularCW,      // G02
    CircularCWW,     // G03
    DrillMode,       // G05
}

impl Display for Mode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            // TODO: Apply correct formatting
            Mode::Route(x, y) => {
                write!(f, "G00X{},{}", x, y)
            }
            Mode::Linear => write!(f, "G01"),
            Mode::CircularCW => write!(f, "G02"),
            Mode::CircularCWW => write!(f, "G03"),
            Mode::DrillMode => write!(f, "G05"),
        }
    }
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

fn parse_fmat(tail: &str) -> Result<LineResult, ExcellonError> {
    run(tail, dec_uint::<_, u8, _>)
        .map(|v| LineResult::Command(Command::FormatCode(v)))
        .map_err(|_| ExcellonError::InvalidVersion)
}

fn parse_ici(tail: &str) -> Result<LineResult, ExcellonError> {
    let opts = opt(preceded(',', alt(("ON", "OFF"))));
    match run(tail, opts) {
        Ok(None) | Ok(Some("ON")) => Ok(LineResult::Command(Command::Incremental(true))),
        Ok(Some("OFF")) => Ok(LineResult::Command(Command::Incremental(false))),
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
        (digit1, '.', digit1)
            .map(|(l, _, t): (&str, _, &str)| (l.len() as u8, t.len() as u8)),
    ));
    let (unit, ty_opt, digits_opt) =
        run(line, (unit, ty, digits)).map_err(|_| ExcellonError::InvalidUnitDefinition)?;
    let default = UnitDefinition::default();
    let (leading, trailing) = digits_opt.unwrap_or((default.leading, default.trailing));
    Ok(LineResult::Command(Command::UnitDefinition(
        UnitDefinition {
            unit,
            ty: ty_opt.unwrap_or(default.ty),
            leading,
            trailing,
        },
    )))
}

fn parse_machine(tail: &str, line: &str) -> Result<LineResult, ExcellonError> {
    let code = run(tail, dec_uint::<_, u8, _>)
        .map_err(|_| ExcellonError::InvalidCmd(line.to_string()))?;
    let mc = match code {
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
        dec_uint::<_, u32, _>,
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
    let id_p = take(2usize).and_then(dec_uint::<_, u8, _>);
    let dwell_p = opt(preceded('X', dec_uint::<_, u16, _>));
    let (id, dwell) =
        run(tail, (id_p, dwell_p)).map_err(|_| ExcellonError::InvalidCmd(line.to_string()))?;
    let code = match id {
        // TODO: Deserialize Coords
        0 => GeometricCode::Mode(Route(0.0, 0.0)),
        1 => GeometricCode::Mode(Mode::Linear),
        2 => GeometricCode::Mode(Mode::CircularCW),
        3 => GeometricCode::Mode(Mode::CircularCWW),
        4 => GeometricCode::VariableDwell(
            dwell.ok_or_else(|| ExcellonError::Custom("Does not start with `G04X`".to_string()))?,
        ),
        5 => GeometricCode::Mode(Mode::DrillMode),
        7 => GeometricCode::OverrideFeed,
        90 => GeometricCode::InputMode(InputMode::Absolute),
        91 => GeometricCode::InputMode(InputMode::Incremental),
        c => return Err(ExcellonError::InvalidGeometricCode(c)),
    };
    Ok(LineResult::Command(Command::Geometric(code)))
}

fn parse_coord(line: &str, fmt: &UnitDefinition) -> Result<LineResult, ExcellonError> {
    fn inner<'i>(
        input: &mut &'i str,
    ) -> ModalResult<(Option<&'i str>, Option<&'i str>)> {
        let val = |i: &mut &'i str| (opt('-'), digit1).take().parse_next(i);
        (opt(preceded('X', val)), opt(preceded('Y', val))).parse_next(input)
    }
    match inner.parse(line) {
        Ok((x, y)) if x.is_some() || y.is_some() => Ok(LineResult::RawCoordinate(
            x.and_then(|t| fmt.parse_num(t).ok()),
            y.and_then(|t| fmt.parse_num(t).ok()),
        )),
        _ => Err(ExcellonError::InvalidCoordinate(line.to_string())),
    }
}

fn parse_line(line: &str, fmt: &UnitDefinition) -> Result<LineResult, ExcellonError> {
    if let Some(stripped) = line.strip_prefix(';') {
        return Ok(LineResult::Command(Command::Comment(stripped.to_string())));
    }
    if line == "%" {
        return Ok(LineResult::Command(Command::Machine(
            MachineCode::RewindStop,
        )));
    }
    if let Some(tail) = line.strip_prefix("FMAT,") {
        return parse_fmat(tail);
    }
    if let Some(tail) = line.strip_prefix("ICI") {
        return parse_ici(tail);
    }
    if line.starts_with("METRIC") || line.starts_with("INCH") {
        return parse_unit(line);
    }
    if let Some(tail) = line.strip_prefix('M') {
        return parse_machine(tail, line);
    }
    if let Some(tail) = line.strip_prefix('T') {
        return parse_tool(tail, line);
    }
    if let Some(tail) = line.strip_prefix('G') {
        return parse_geometric(tail, line);
    }
    if line.starts_with('X') || line.starts_with('Y') {
        return parse_coord(line, fmt);
    }
    Err(ExcellonError::InvalidCmd(line.to_string()))
}

pub fn parse_excellon<T>(mut reader: BufReader<T>) -> std::io::Result<ExcellonLayerData>
where
    T: std::io::Read,
{
    let mut commands = Vec::new();

    let mut format = UnitDefinition::default();

    let mut buf = String::new();
    let mut line_number = 0;
    let mut tools = HashMap::new();
    while reader.read_line(&mut buf)? > 0 {
        let trimmed = buf.trim();
        let cmd_result = match parse_line(trimmed, &format) {
            Ok(LineResult::Command(cmd)) => {
                match &cmd {
                    Command::UnitDefinition(unit) => format = unit.clone(),
                    Command::Machine(MachineCode::Scale(u)) => format.unit = *u,
                    Command::ToolDefinition(td) => {
                        tools.insert(td.tool_number, td.diameter);
                    }
                    Command::Tool(id) => {
                        if !tools.contains_key(id) {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                ExcellonError::InvalidToolNumber(*id),
                            ));
                        }
                    }
                    _ => {}
                }
                Ok(cmd)
            }
            Ok(LineResult::RawCoordinate(x, y)) => {
                Ok(Command::Coordinate(x, y, format.clone()))
            }
            Err(err) => Err(err),
        };
        commands.push(cmd_result.map_err(|e| ExcellonParseFormat {
            source: e,
            line: line_number,
            content: trimmed.to_string(),
        }));
        buf.clear();
        line_number += 1;
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
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            ExcellonError::MissingHeaderEnd,
        ));
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
    fn test_leading_trailing() -> Result<(), Box<dyn std::error::Error>> {
        let fmt = UnitDefinition {
            leading: 3,
            trailing: 3,
            ty: ZeroSuppression::Leading,
            unit: Unit::Millimeters,
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