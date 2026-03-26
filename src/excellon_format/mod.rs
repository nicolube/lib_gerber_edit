use crate::excellon_format::Mode::Route;
use crate::unit_able::UnitAble;
use crate::{LayerData, LayerMerge, LayerStepAndRepeat, LayerTransform, Pos};
use derive_more::{Display, Error};
use gerber_parser::gerber_types::Unit;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::num::{ParseFloatError, ParseIntError};
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq)]
pub struct ExcellonLayerData {
    pub header: Vec<Result<Command, ExcellonParseFormat>>,
    pub commands: Vec<Result<Command, ExcellonParseFormat>>,
    pub unit: UnitDefinition,
    pub tools: HashMap<u32, f64>,
}

impl ExcellonLayerData {
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
    #[display("Invalid tool number: {}", _0)]
    InvalidToolNumber(#[error(not(source))] u32),
    #[display("Failed to parse floating number: {}", _1)]
    FloatParse(#[error(source)] ParseFloatError, String),
    #[display("Failed to parse number: {}", _1)]
    IntParse(#[error(source)] ParseIntError, String),
    #[display("Failed to parse version number")]
    InvalidVersion,
    #[display("{}", _0)]
    Custom(#[error(not(source))] String),
}

#[derive(Debug, Clone, Error, PartialEq, Display)]
#[display("Excellon parse error at line {}: {}", line, content)]
pub struct ExcellonParseFormat {
    #[error(source)]
    source: ExcellonError,
    line: usize,
    content: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    FormatCode(u8),
    Incremental(bool),
    UnitDefinition(UnitDefinition),
    Geometric(GeometricCode),
    Machine(MachineCode),
    Coordinate(Option<f64>, Option<f64>, UnitDefinition),
    Tool(u32),
    ToolDefinition(ToolDefinition),
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
    unit: Unit,
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

impl FromStr for GeometricCode {
    type Err = ExcellonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.chars().nth(0) != Some('G') {
            return Err(ExcellonError::InvalidCmd(s.to_string()));
        }
        let id = s[1..3]
            .parse::<u8>()
            .map_err(|e| ExcellonError::IntParse(e, s.to_string()))?;
        match id {
            // TODO: Deserialize Coords
            0 => Ok(GeometricCode::Mode(Route(0.0, 0.0))),
            1 => Ok(GeometricCode::Mode(Mode::Linear)),
            2 => Ok(GeometricCode::Mode(Mode::CircularCW)),
            3 => Ok(GeometricCode::Mode(Mode::CircularCWW)),
            4 => {
                if s.chars().nth(3) != Some('X') {
                    Err(ExcellonError::Custom(
                        "Does not start with `G04X`".to_string(),
                    ))?;
                }
                let duration = s[4..]
                    .parse::<u16>()
                    .map_err(|e| ExcellonError::IntParse(e, s[4..].to_string()))?;
                Ok(GeometricCode::VariableDwell(duration))
            }
            5 => Ok(GeometricCode::Mode(Mode::DrillMode)),
            7 => Ok(GeometricCode::OverrideFeed),
            90 => Ok(GeometricCode::InputMode(InputMode::Absolute)),
            91 => Ok(GeometricCode::InputMode(InputMode::Incremental)),
            _ => Err(ExcellonError::InvalidGeometricCode(id)),
        }
    }
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
    RawCoordinate(Option<String>, Option<String>),
}
impl FromStr for LineResult {
    type Err = ExcellonError;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        if let Some(stripped) = line.strip_prefix(';') {
            // Comment line
            Ok(LineResult::Command(Command::Comment(stripped.to_string())))
        } else if let Some(version) = line.strip_prefix("FMAT,") {
            let version = version.parse().map_err(|_| ExcellonError::InvalidVersion)?;
            Ok(LineResult::Command(Command::FormatCode(version)))
        } else if let Some(options) = line.strip_prefix("ICI") {
            if options.is_empty() || options == ",ON" {
                Ok(LineResult::Command(Command::Incremental(true)))
            } else if options == ",OFF" {
                Ok(LineResult::Command(Command::Incremental(false)))
            } else {
                Err(Self::Err::InvalidCicOption(options.to_owned()))
            }
        } else if line == "%" {
            Ok(LineResult::Command(Command::Machine(
                MachineCode::RewindStop,
            )))
        } else if line.starts_with("METRIC") || line.starts_with("INCH") {
            // Unit definition
            let parts: Vec<&str> = line.split(',').collect();
            let unit = if line.is_empty() || parts[0] == "INCH" {
                Unit::Inches
            } else if parts[0] == "METRIC" {
                Unit::Millimeters
            } else {
                return Err(ExcellonError::InvalidUnitDefinition);
            };

            let zero_suppression = if parts.len() >= 2 {
                if parts[1] == "TZ" {
                    ZeroSuppression::Trailing
                } else if parts[1] == "LZ" {
                    ZeroSuppression::Leading
                } else {
                    return Err(ExcellonError::InvalidUnitDefinition);
                }
            } else {
                UnitDefinition::default().ty
            };
            let (leading, trailing) = if parts.len() >= 3
                && let Some((leading, trailing)) = parts[2].split_once(".")
            {
                (leading.len() as u8, trailing.len() as u8)
            } else {
                let default = UnitDefinition::default();
                (default.leading, default.trailing)
            };
            Ok(LineResult::Command(Command::UnitDefinition(
                UnitDefinition {
                    unit,
                    ty: zero_suppression,
                    trailing,
                    leading,
                },
            )))
        } else if let Some(code) = line.strip_prefix('M') {
            // Machine code
            let code = code.parse::<u8>().unwrap_or(0);
            let code = match code {
                30 => MachineCode::EndOfProgram,
                48 => MachineCode::HeaderStart,
                71 => MachineCode::Scale(Unit::Millimeters),
                72 => MachineCode::Scale(Unit::Inches),
                95 => MachineCode::HeaderEnd,
                code => return Err(ExcellonError::InvalidMachineCode(code)),
            };
            Ok(LineResult::Command(Command::Machine(code)))
        } else if line.starts_with('T') {
            // T<tool_number> or T<tool_number>C<diameter>
            let parts: Vec<&str> = line.trim().split('C').collect();

            if parts.is_empty() || parts.len() > 2 {
                return Err(ExcellonError::InvalidToolDefinition(line.to_string()));
            }
            let tool_number = parts[0][1..]
                .parse::<u32>()
                .map_err(|err| ExcellonError::IntParse(err, parts[0][1..].to_string()))?;

            if parts.len() == 2 {
                // Tool definition with diameter
                let diameter = parts[1]
                    .parse::<f64>()
                    .map_err(|err| ExcellonError::FloatParse(err, parts[1][1..].to_string()))?;
                Ok(LineResult::Command(Command::ToolDefinition(
                    ToolDefinition {
                        tool_number,
                        diameter,
                    },
                )))
            } else if parts.len() == 1 {
                // Tool selection
                Ok(LineResult::Command(Command::Tool(tool_number)))
            } else {
                Err(ExcellonError::InvalidCmd(line.to_string()))
            }
        } else if line.starts_with('G') {
            Ok(LineResult::Command(Command::Geometric(
                GeometricCode::from_str(line)?,
            )))
        } else if line.starts_with("X") || line.starts_with("Y") {
            let mut x = None;
            let mut y = None;
            let mut pos = 0;
            for _ in 0..2 {
                let num = line[pos + 1..]
                    .chars()
                    .take_while(|x| x.is_numeric() || x == &'-')
                    .collect::<String>();
                let len = num.len() + 1;
                match line.chars().nth(pos) {
                    Some('X') => x = Some(num),
                    Some('Y') => y = Some(num),
                    _ => return Err(ExcellonError::InvalidCoordinate(line.to_string())),
                }
                pos += len;
            }
            if x.is_some() || y.is_some() {
                Ok(LineResult::RawCoordinate(x, y))
            } else {
                Err(ExcellonError::InvalidCoordinate(line.to_string()))
            }
        } else {
            Err(ExcellonError::InvalidCmd(line.to_string()))
        }
    }
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
        let line = buf.clone();
        let result = LineResult::from_str(line.trim());
        commands.push(
            match result {
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
                Ok(LineResult::RawCoordinate(x, y)) => Ok(Command::Coordinate(
                    x.and_then(|t| format.parse_num(t.as_str()).ok()),
                    y.and_then(|t| format.parse_num(t.as_str()).ok()),
                    format.clone(),
                )),
                Err(err) => Err(err),
            }
            .map_err(|e| ExcellonParseFormat {
                source: e,
                line: line_number,
                content: line.clone(),
            }),
        );
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
    header.push(commands[header.len()].clone());
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

impl From<ExcellonLayerData> for LayerData {
    fn from(value: ExcellonLayerData) -> Self {
        LayerData::Excellon(value)
    }
}
